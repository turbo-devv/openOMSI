//! The window's events: winit's `ApplicationHandler` for `App`.

/// Mirror pictures drawn per second at most, all mirrors together (see the redraw).
const MIRROR_RATE: f32 = 75.0;
/// The least a mirror is redrawn a second (see the mirrors in `window_event`).
const MIRROR_MIN_HZ: f32 = 8.0;
/// The most a mirror in the picture is redrawn a second, with the real-time reflections
/// economical (`mirror_refresh=eco`) and full (the default).
const MIRROR_MAX_HZ_ECO: f32 = 15.0;
const MIRROR_MAX_HZ_FULL: f32 = 30.0;
/// With no real-time reflections (`mirror_refresh=off`) a bus's mirrors are drawn once when
/// it is taken over and once more this many seconds later.
const MIRROR_FREEZE_REDRAW: f32 = 2.0;

/// Consume the VR redraw budget without updating a mirror twice in one frame.
/// Negative rates request every mirror each frame; zero freezes immediately.
fn vr_mirror_updates(budget: &mut f32, dt: f32, rate: f32, mirrors: usize) -> usize {
    if mirrors == 0 || rate == 0.0 {
        *budget = 0.0;
        return 0;
    }
    if rate < 0.0 {
        *budget = 0.0;
        return mirrors;
    }
    // Keep only a frame's worth of work after a stall, with the fractional
    // credit carried forward for rates below the game's frame rate.
    *budget = (*budget + dt.clamp(0.0, 0.1) * rate).min(mirrors as f32 + 0.5);
    let updates = (budget.floor() as usize).min(mirrors);
    *budget -= updates as f32;
    updates
}

fn render_scale_step(fps: f32, slow_frame_wait_share: f32) -> f32 {
    // (three levels, far apart, and a wide band between going down and up again: every
    // step makes the picture's targets anew - hundreds of MB with MSAA and HDR - and a
    // scale that went up and down by 5 % every two seconds stuttered at each change and
    // filled the card's memory with the old ones until the driver gave up)
    if fps < 40.0 && slow_frame_wait_share >= 0.4 {
        -0.15
    } else if fps > 58.0 || slow_frame_wait_share < 0.2 {
        0.15
    } else {
        0.0
    }
}

use super::*;

/// How fast a stick turns the head, fully pushed (degrees a second, see `Analog::look`).
const LOOK_STICK_DEG_S: f32 = 120.0;

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.resumed_impl(event_loop);
    }

    /// A phone put the app into the background: its window's surface goes (made again on
    /// `resumed`), the fingers and the held keys are let go.
    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        self.surface = None;
        self.touch.drop_gpu();
        self.input_lost();
        self.save_last_situation();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.finish_vr_nav_edit();
                self.finish_session();
                crate::platform::exit(event_loop);
            }
            WindowEvent::Resized(size) => {
                if let (Some(s), Some(r)) = (self.surface.as_mut(), self.renderer.as_ref()) {
                    s.resize(r, size.width, size.height);
                }
                // (minimised, Windows makes the window 0 x 0)
                let hidden = size.width == 0 || size.height == 0;
                if hidden && !self.window_hidden {
                    self.input_lost();
                }
                self.window_hidden = hidden;
            }
            // (minimised or covered entirely, macOS and Wayland: the focus goes with it, and a
            // window merely covered by another may still be the one the player drives with)
            WindowEvent::Occluded(hidden) => self.window_hidden = hidden,
            WindowEvent::Focused(true) => {
                self.window_focused = true;
                if let Some(ctl) = self.controllers.as_mut() {
                    ctl.set_focus(true);
                }
                self.input_back();
            }
            WindowEvent::Focused(false) => {
                self.finish_vr_nav_edit();
                self.window_focused = false;
                if let Some(ctl) = self.controllers.as_mut() {
                    ctl.set_focus(false);
                }
                #[cfg(windows)]
                {
                    self.vr_cursor_physical = None;
                    self.vr_cursor_warp_pending = None;
                }
                // No key-up reaches us for whatever was held when focus left (alt-tab, a
                // click outside the window, an OS dialog popping up): without this, a held
                // modifier got "stuck" and made the next plain key press look like it was
                // held with that modifier - Shift got stuck this way once, and a plain `W`
                // (throttle in the wasd preset) was then read as Shift+W, OMSI's own wiper
                // key, toggling the wipers on every press instead of driving. The vehicle's
                // own keys, a door button, a switch held with the mouse and the mouse
                // steering's throttle went on as well, with the window minimised.
                self.input_lost();
            }
            // nothing the keyboard or the mouse does reaches the game while the window is
            // in the background (a wheel turned over a window behind another one zoomed;
            // the keys a system sends again for what is still held when the focus comes
            // back count only when pressed anew)
            WindowEvent::KeyboardInput { is_synthetic, ref event, .. } if self.input_away || (is_synthetic && event.state == ElementState::Pressed) => {}
            WindowEvent::MouseInput { .. } | WindowEvent::MouseWheel { .. } if self.input_away => {}
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed && self.menu_edit_icao {
                    if let Some(text)=event.text.as_deref(){ self.icao_edit_text(text); }
                }
                // Route numbers are free display text in OMSI. Take the text produced by
                // the keyboard layout (rather than only the physical key) so '-', shifted
                // symbols and non-US layouts reach the destination display unchanged.
                if event.state == ElementState::Pressed
                    && self.menu_edit.is_some()
                    && !self.menu_edit_icao
                    && matches!(self.list_kind, Some(crate::game_lists::ListKind::RouteNumbers))
                {
                    if let Some(text) = event.text.as_deref() {
                        if text.chars().any(|c| !c.is_control()) {
                            self.route_edit_text(text);
                            return;
                        }
                    }
                }
                // '/' opens the chat's input box wherever the keyboard has it (the key
                // itself is then swallowed by the chat) - but not Numpad ÷, OMSI's stock
                // front door key (keyboard.cfg `bus_doorfront0 181`)
                // (only while `chat_open` is on its own key: one the player moved it to is
                // the only one, #130)
                if event.state == ElementState::Pressed
                    && event.text.as_deref() == Some("/")
                    && event.physical_key != PhysicalKey::Code(KeyCode::NumpadDivide)
                    && self.game_keys.iter().any(|b| b.action.eq_ignore_ascii_case("chat_open") && b.scan_code == 53 && b.chord() == 0)
                    && self.lan.is_some()
                    && !lan::chat_open(&self.remotes)
                {
                    self.remotes.chat.open();
                    if let PhysicalKey::Code(code) = event.physical_key {
                        lan::chat_swallow(&mut self.remotes, code);
                    }
                    return;
                }
                // what is typed into an open LAN chat line (the key itself goes on to on_key)
                if let (Some(text), true, true) = (
                    event.text.as_deref(),
                    event.state == ElementState::Pressed,
                    lan::chat_open(&self.remotes),
                ) {
                    lan::chat_type(&mut self.remotes, text);
                }
                // (the Lua plugins' `key` event; a key held down repeats nothing)
                if let (PhysicalKey::Code(code), false) = (event.physical_key, event.repeat) {
                    if self.plugin_keys.len() < 64 {
                        self.plugin_keys.push((format!("{code:?}"), event.state == ElementState::Pressed));
                    }
                }
                // a phone's back key is Escape (the game menu, out of the city map ...)
                let physical = match event.physical_key {
                    PhysicalKey::Code(KeyCode::BrowserBack) => PhysicalKey::Code(KeyCode::Escape),
                    k => k,
                };
                if let PhysicalKey::Code(code) = physical {
                    self.on_key(
                        event_loop,
                        code,
                        event.state == ElementState::Pressed,
                        event.repeat,
                    );
                }
            }
            // In VR right-click zooms; with mouse steering it first releases the steering.
            // On the desktop a right-drag zooms, as in OMSI (`on_right`).
            WindowEvent::MouseInput {
                state,
                button: winit::event::MouseButton::Right,
                ..
            } => {
                if let Some(edit) = self.vr_nav_edit.as_mut() {
                    edit.rotating = state == ElementState::Pressed;
                    return;
                }
                if self.navigator.as_ref().map(|n| n.map_open()).unwrap_or(false) {
                    return;
                }
                if self.vr_active() {
                    #[cfg(windows)]
                    if state == ElementState::Pressed && self.game_menu.is_none()
                        && self.chooser.is_none() {
                        if self.mouse_drive {
                            self.set_mouse_drive(false);
                            self.service_msg = Some(("Mouse steering off".into(), 3.0));
                        } else {
                            self.vr_zoom_active = !self.vr_zoom_active;
                        }
                    }
                } else {
                    self.on_right(state == ElementState::Pressed);
                }
            }
            // (the middle button - the wheel pressed - turns the view as well: OMSI's pan)
            WindowEvent::MouseInput {
                state,
                button: winit::event::MouseButton::Middle,
                ..
            } => {
                if self.vr_nav_edit.is_some() { return; }
                if self.navigator.as_ref().map(|n| n.map_open()).unwrap_or(false) {
                    return;
                }
                self.mouse_look = state == ElementState::Pressed;
                self.update_hover();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let amount = match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, y) => y,
                    winit::event::MouseScrollDelta::PixelDelta(p) => p.y as f32 / 40.0,
                };
                self.wheel(amount);
            }
            WindowEvent::CursorMoved { position, .. } => {
                if self.vr_nav_edit.is_some() { return; }
                // (both physical pixels)
                if let Some((x, y)) = self.cursor_hidden {
                    if (position.x as f32 - x).abs() + (position.y as f32 - y).abs() > 8.0 {
                        self.cursor_hidden = None;
                        if let Some(win) = self.window.as_ref() {
                            win.set_cursor_visible(true);
                        }
                    }
                }
                // (the on-screen controls on a computer, `OMSI_TOUCH=1`: the mouse is a
                // finger on them - from #202)
                if self.touch.enabled {
                    self.finger_move(0, glam::Vec2::new(position.x as f32, position.y as f32));
                }
                #[cfg(windows)]
                let vr_cockpit = self.vr.is_some() && self.game_menu.is_none()
                    && matches!(self.view.as_str(), "driver" | "pax");
                #[cfg(not(windows))]
                let vr_cockpit = false;
                if vr_cockpit && !self.mouse_look && !self.mouse_drive {
                    #[cfg(windows)]
                    self.on_vr_cursor_moved(position.x as f32, position.y as f32);
                } else {
                    self.on_mouse_moved(position.x as f32, position.y as f32);
                }
            }
            WindowEvent::MouseInput {
                state,
                button: winit::event::MouseButton::Left,
                ..
            } => {
                if self.touch.enabled {
                    let p = glam::Vec2::new(self.cursor.0, self.cursor.1);
                    if state == ElementState::Pressed {
                        self.finger_down(event_loop, 0, p);
                    } else {
                        self.finger_up(event_loop, 0, p, false);
                    }
                } else {
                    let pressed = state == ElementState::Pressed;
                    self.buttons_held.0 = pressed;
                    // the right button already down (looking round): both held zoom, and
                    // the click works nothing in the cab
                    if pressed && self.buttons_held.1 && self.start_both_drag() {
                        return;
                    }
                    // (the right button still held goes on zooming by itself, unless with
                    // `[altView]` it turns the view)
                    if !pressed && self.both_drag.is_some() && !(self.buttons_held.1 && self.right_zooms()) {
                        self.both_drag = None;
                        self.mouse_look = self.buttons_held.1;
                        self.update_hover();
                    }
                    self.left_button(event_loop, pressed);
                }
            }
            // a finger (a phone; see touch.rs)
            WindowEvent::Touch(t) => self.on_touch(event_loop, t),
            WindowEvent::RedrawRequested => {
                if self.vr_nav_edit.is_some() && (!self.vr_active() || self.view != "driver") {
                    self.finish_vr_nav_edit();
                }
                if (!self.vr_active() || self.player.is_none())
                    && matches!(self.list_kind, Some(crate::game_lists::ListKind::Options(_)))
                    && self.admin_list.as_ref().is_some_and(|rows| rows.iter().any(|(_, action)| action.starts_with("vr_nav_")))
                {
                    self.open_list(crate::game_lists::ListKind::Options(0));
                }
                #[cfg(windows)]
                self.poll_vr_cursor_position();
                // OMSI's autosave of the last situation: every five minutes of play
                if !self.paused && self.player.is_some() && self.clock.run_time - self.autosave_t >= 300.0 {
                    self.autosave_t = self.clock.run_time;
                    self.save_last_situation();
                }
                // the time of day a script set last frame (the nearer way round the clock)
                // (not with the real-time sync on: the clock stays the device's)
                if let Some(t) = self.pending_time.take().filter(|_| !self.real_time_locked()) {
                    let d = (t - self.clock.time + 43_200.0).rem_euclid(86_400.0) - 43_200.0;
                    self.shift_clock(d);
                }
                // the graphics device is gone (a driver reset, an external card unplugged):
                // nothing can be drawn again - end the session the ordinary way, so that the
                // summary, the personnel file and the LAN goodbye are not lost
                // the card ran out of memory: fewer textures (the finest levels of the far
                // ones go), before the driver gives the device up
                if self.renderer.as_ref().is_some_and(|r| r.take_out_of_memory()) {
                    if let Some(w) = self.world.as_ref() {
                        let now = w.texture_budget_bytes();
                        let less = if now == 0 { 600_000_000 } else { (now * 3 / 5).max(300_000_000) };
                        w.set_texture_budget(less);
                        log::warn!("the graphics card ran out of memory: textures kept to {:.0} MB from now on", less as f64 / 1e6);
                    }
                }
                if let Some(why) = self.renderer.as_ref().and_then(|r| r.device_lost()) {
                    if self.restart_after_device_loss() {
                        log::warn!("device lost ({why}): the game goes on in a new start");
                    } else {
                        log::error!("ending the session: the graphics device was lost ({why})");
                    }
                    crate::platform::exit(event_loop);
                    return;
                }
                let desktop_vsync = self.settings.vsync && !self.vr_active();
                if let (Some(surface), Some(renderer)) = (self.surface.as_mut(), self.renderer.as_ref()) {
                    surface.set_vsync(renderer, desktop_vsync);
                }
                // in the own bus's cab: at the wheel, a passenger's view, or sitting in a
                // seat of it after getting up (its inside is drawn and heard from inside)
                self.in_cab = matches!(self.view.as_str(), "driver" | "pax")
                    || (self.view == "foot" && self.foot_bus() == Some(crate::humans::BusId::Player));
                // standing or sitting in another player's bus: that bus is drawn and heard
                // from inside (its interior meshes, not the outside ones over them)
                self.inside_remote = match self.foot_bus() {
                    Some(crate::humans::BusId::Ai(x)) if self.view == "foot" => crate::humans::remote_bus_player(x),
                    _ => None,
                };
                let now = Instant::now();
                let raw_dt = (now - self.last).as_secs_f32();
                self.log_frame(raw_dt);
                let profiling = omsi_cfg::env::var_os("OMSI_PROFILE").is_some();
                let waited: f64 = ["acquire", "present", "gpu"].iter()
                    .map(|&k| self.profile.get(k).copied().unwrap_or(0.0))
                    .sum();
                let wait_this_frame = (waited - self.governor_wait_prev).max(0.0) as f32;
                self.governor_wait_prev = waited;
                if self.total_frames > 60 {
                    if raw_dt > 0.05 {
                        self.spikes += 1;
                        if profiling {
                            // where the slow frame went: the stages that took more than 2 ms,
                            // and what no stage accounts for (waiting for the window, the
                            // system, other processes)
                            let mut parts: Vec<(&'static str, f64)> = self
                                .profile
                                .iter()
                                .map(|(k, v)| {
                                    (*k, v - self.profile_prev.get(k).copied().unwrap_or(0.0))
                                })
                                .collect();
                            let staged: f64 = parts
                                .iter()
                                .filter(|(k, _)| !k.contains('.'))
                                .map(|p| p.1)
                                .sum();
                            parts.retain(|p| p.1 > 0.002);
                            parts.sort_by(|a, b| b.1.total_cmp(&a.1));
                            let list: Vec<String> = parts
                                .iter()
                                .map(|(k, v)| format!("{k} {:.0}", v * 1000.0))
                                .collect();
                            log::info!(
                                "stutter: frame {} took {:.0} ms ({}; outside the stages {:.0} ms)",
                                self.total_frames,
                                raw_dt * 1000.0,
                                list.join(", "),
                                (raw_dt as f64 - staged).max(0.0) * 1000.0
                            );
                        }
                    }
                    self.worst_ms = self.worst_ms.max(raw_dt * 1000.0);
                    // Reduce resolution only when slow frames spend substantial time waiting
                    // for presentation or the GPU. Traffic, scripts and tile work can drop
                    // the frame rate too, but fewer pixels cannot make those stages faster.
                    // Keep the player's chosen scale and explicit fixed-scale override.
                    // A fast V-synced frame can wait for the next refresh without being
                    // GPU-bound. Count presentation wait only on slow frames.
                    if raw_dt > 0.02 {
                        self.governor.2 += wait_this_frame;
                    }
                    self.governor.0 += raw_dt;
                    self.governor.1 += 1;
                    if self.governor.0 >= 5.0 {
                        let fps = self.governor.1 as f32 / self.governor.0;
                        let wait_share = self.governor.2 / self.governor.0;
                        self.governor = (0.0, 0, 0.0);
                        let free = self.settings.render_scale <= 0.0
                            && (self.settings.max_fps == 0 || self.settings.max_fps >= 50)
                            && omsi_cfg::env::var_os("OMSI_FIXED_SCALE").is_none();
                        if let (Some(r), true) = (self.renderer.as_mut(), free) {
                            let s = r.dynamic_scale();
                            let step = render_scale_step(fps, wait_share);
                            r.set_dynamic_scale(s + step);
                            if (r.dynamic_scale() - s).abs() > 1e-3 {
                                log::info!("frame rate {fps:.0} fps (presentation wait {:.0}%): the 3D picture is drawn at {:.0} % of the window now", wait_share * 100.0, r.dynamic_scale() * 100.0);
                                self.governor_low = 0;
                            } else if step < 0.0 {
                                // at the smallest scale and still waiting for the card: after
                                // two such readings the picture gets lighter itself (a weak or
                                // old graphics chip keeps a playable frame rate)
                                self.governor_low += 1;
                                if self.governor_low >= 2 && fps < 30.0 {
                                    self.governor_low = 0;
                                    // (SSAO first, then the shadows; for this drive only)
                                    let what = r.lighten().or_else(|| {
                                        std::mem::replace(&mut self.settings.shadows, false).then_some("shadows off")
                                    });
                                    if let Some(what) = what {
                                        log::warn!("frame rate {fps:.0} fps at the smallest render scale: {what} to keep up");
                                        self.service_msg = Some((format!("The graphics card cannot keep up: {what}"), 4.0));
                                    }
                                }
                            }
                        }
                    }
                }
                if profiling {
                    self.profile_prev.clone_from(&self.profile);
                }
                let dt = raw_dt.min(0.1);
                self.last = now;
                // (the cursor over the game menu: a hand over what can be clicked)
                if self.game_menu.is_some() {
                    let kind = self.menu_cursor_kind();
                    self.set_cursor_kind(kind);
                }
                self.run_input_script(event_loop);
                if let Some(m) = self.menu.as_ref() {
                    if let Some(limit) = self.args.exit_after {
                        if self.started.elapsed().as_secs_f32() > limit {
                            log::info!(
                                "menu: {} maps, {} vehicles",
                                m.maps.len(),
                                m.vehicles.len()
                            );
                            crate::platform::exit(event_loop);
                        }
                    }
                    let lines = m.lines();
                    if let (Some(hud), Some(s), Some(r), Some(scene), Some(win)) = (
                        self.hud.as_mut(),
                        self.surface.as_ref(),
                        self.renderer.as_mut(),
                        self.scene.as_mut(),
                        self.window.as_ref(),
                    ) {
                        hud.update(r, scene, &lines);
                        if let wgpu::CurrentSurfaceTexture::Success(frame)
                        | wgpu::CurrentSurfaceTexture::Suboptimal(frame) =
                            s.surface.get_current_texture()
                        {
                            let view = frame.texture.create_view(&Default::default());
                            let cam = Camera {
                                position: DVec3::ZERO,
                                yaw: 0.0,
                                pitch: 0.0,
                                roll: 0.0,
                                fov_deg: 60.0,
                                near: 0.5,
                                far: 100.0,
                            };
                            let lighting = omsi_render::Lighting {
                                sky_color: glam::Vec3::new(0.08, 0.10, 0.14),
                                ..Default::default()
                            };
                            r.render(
                                scene,
                                &view,
                                s.config.width,
                                s.config.height,
                                &cam,
                                &lighting,
                            );
                            win.pre_present_notify();
                            frame.present();
                        }
                    }
                    return;
                }
                if !self.drive_start(event_loop) {
                    // the session goes on while the map loads: a big map's first area took
                    // longer than the host waits for a silent player
                    if let Some(l) = self.lan.as_mut() {
                        let planned = omsi_net::Pose {
                            bus: self.args.bus.clone().unwrap_or_default().replace('\\', "/"),
                            ..Default::default()
                        };
                        l.keepalive(dt, &planned);
                    }
                    return;
                }
                let __t = Instant::now();
                self.drive_streaming();
                *self.profile.entry("streaming").or_default() += __t.elapsed().as_secs_f64();
                let __t = Instant::now();
                // (with a triple screen, all of its three panels are in sight)
                let sight = self.camera.as_ref().zip(self.surface.as_ref())
                    .and_then(|(c, s)| self.sight_extent(c, (s.config.width, s.config.height)));
                if let (Some(t), Some(w), Some(r), Some(scene)) = (
                    self.traffic.as_mut(),
                    self.world.as_ref(),
                    self.renderer.as_ref(),
                    self.scene.as_mut(),
                ) {
                    let center = self
                        .player
                        .as_ref()
                        .map(|p| p.vehicle.position)
                        .or(self.camera.as_ref().map(|c| c.position))
                        .unwrap_or(DVec3::ZERO);
                    let aspect = self
                        .surface
                        .as_ref()
                        .map(|s| s.config.width as f64 / s.config.height.max(1) as f64)
                        .unwrap_or(16.0 / 9.0);
                    let fog = self
                        .weather
                        .as_ref()
                        .map(|w| w.fog.0 as f64)
                        .unwrap_or(50000.0);
                    traffic_inputs(
                        t,
                        self.camera.as_ref(),
                        aspect,
                        sight,
                        fog,
                        &self.clock,
                        self.humans.as_ref(),
                        self.player.as_ref(),
                        &r.options,
                    );
                    self.populate_t -= dt;
                    if self.populate_t <= 0.0 && !self.paused {
                        // come back quickly while there is a backlog of departures to put out
                        self.populate_t = if self
                            .schedule
                            .as_ref()
                            .map(|s| s.pending() > 0)
                            .unwrap_or(false)
                        {
                            0.1
                        } else {
                            2.0
                        };
                        let __t5 = Instant::now();
                        let view = self.camera.as_ref().map(|c| c.forward().as_dvec3());
                        t.populate_seen(w, r, scene, center, view);
                        *self.profile.entry("traffic.populate").or_default() +=
                            __t5.elapsed().as_secs_f64();
                        t.keep_clear = self
                            .player
                            .as_ref()
                            .map(|p| traffic::vehicle_bodies(&p.vehicle))
                            .unwrap_or_default();
                        t.keep_clear.extend(
                            self.remotes
                                .remotes
                                .values()
                                .flat_map(|r| traffic::vehicle_bodies(r.vehicle())),
                        );
                        if let Some(s) = self.schedule.as_mut() {
                            let window = if self.first_populate {
                                20.0 * 60.0
                            } else {
                                2.5
                            };
                            let __t6 = Instant::now();
                            s.tick(w, t, r, scene, t.day_time, window);
                            *self.profile.entry("traffic.schedule").or_default() +=
                                __t6.elapsed().as_secs_f64();
                        }
                        self.first_populate = false;
                    }
                    // the AI's lights (and a bus's saloon lamps, which its scripts switch with
                    // them): by the time of day, and by day in fog, rain, snow or under a
                    // closed cloud cover as drivers do
                    let gloomy = self.weather.as_ref().map(|w| {
                        let (kind, rate) = precip_of(w);
                        w.fog.0 < 600.0 || (kind != 0 && rate > 0.05) || w.clouds.0.trim().to_ascii_lowercase().starts_with("overcast")
                    }).unwrap_or(false);
                    // Omsi switches the AI's lights on below a light value of 0.75, before
                    // the street lamps (0.6), and off after them in the morning
                    let daylight = omsi_sim::Daylight::compute(&self.clock, self.envir.as_ref());
                    t.night = daylight.brightness < 0.75 || gloomy;
                    t.daylight = Some(daylight);
                    let __t2 = Instant::now();
                    t.others = lan_outlines(&self.remotes);
                    t.others.extend(own_outlines(self.player.as_ref(), &self.placed));
                    if !self.paused {
                        t.player_priority = self.player.as_ref().and_then(|p| p.vehicle.var("TrafficPriority")).is_some_and(|v| v > 0.5);
                        t.player_blinker = self.player.as_ref().map(|p| lan::indicator(&p.vehicle)).unwrap_or(0);
                        t.tick(dt, self.player.as_ref().map(|p| player_outline(p)));
                        if let Some(w) = self.world.as_ref() {
                            w.set_switches(&t.switch_requests());
                            let rail = self.player.as_ref().and_then(|p| p.rail.as_ref()).map(|r| (r.lane, r.along));
                            w.set_signals(&t.signal_aspects(&w.signal_routes, rail));
                        }
                    }
                    *self.profile.entry("traffic.tick").or_default() +=
                        __t2.elapsed().as_secs_f64();
                    for (k, v) in ["traffic.tick.lanes", "traffic.tick.plan", "traffic.tick.ai"]
                        .into_iter()
                        .zip(t.tick_split)
                    {
                        *self.profile.entry(k).or_default() += v;
                    }
                    if let Some(p) = self.player.as_mut() {
                        // the options' [no_collision_vehToVeh]: the bus drives through the traffic
                        p.vehicle.dynamic_boxes = if self.settings.collision_vehicles { t.boxes(p.vehicle.position, 80.0) } else { Vec::new() };
                    }
                    let __t3 = Instant::now();
                    if let Some(a) = self.audio.as_ref() {
                        let street = self
                            .weather
                            .as_ref()
                            .map(|w| street_condition(w, self.wetness))
                            .unwrap_or(0.0);
                        let muffled = self.in_cab;
                        // heard round the camera (the ear), not round the player's bus: a
                        // free camera following an AI bus lost its sound 250 m from the bus
                        let ear = self.camera.as_ref().map(|c| c.position).unwrap_or(center);
                        t.update_audio(a, ear, street, muffled);
                    }
                    *self.profile.entry("traffic.audio").or_default() +=
                        __t3.elapsed().as_secs_f64();
                    let __t4 = Instant::now();
                    t.camera = self.camera.as_ref().map(|c| c.position);
                    t.sync(w, r, scene);
                    *self.profile.entry("traffic.sync").or_default() +=
                        __t4.elapsed().as_secs_f64();
                }
                *self.profile.entry("traffic").or_default() += __t.elapsed().as_secs_f64();
                // The player's vehicle moves before the passengers are placed: they sit in
                // the bus frame, and placing them on the pose of the frame before made everyone
                // aboard tremble at speed (a quarter of a metre behind the seat, every frame).
                let __t = Instant::now();
                self.drag_frame();
                // the tutorial's pages, once the world is there
                if self.world.is_some() {
                    if let Some(n) = self.args.tutorial.take() {
                        self.tutorial = crate::tutorial::Tutorial::load(&self.args.root, n, &self.settings.language);
                    }
                }
                // the game controllers: their axes this frame, their buttons' key actions
                let hwnd = self.window.as_deref().and_then(crate::controllers::window_handle);
                let ctl = self.controllers.get_or_insert_with(|| crate::controllers::Controllers::new(&self.args.root, hwnd));
                ctl.set_focus(self.window_focused);
                ctl.deadzone = self.settings.ctrl_deadzone;
                ctl.pedal_throttle = self.settings.pedal_throttle;
                ctl.pedal_brake = self.settings.pedal_brake;
                ctl.ff_invert = self.settings.ff_invert;
                ctl.ff_enabled = self.settings.ff_enabled;
                ctl.steer_gain = if self.settings.wheel_lock >= 45.0 { (self.settings.wheel_range / self.settings.wheel_lock).clamp(0.1, 20.0) } else { 1.0 };
                if ctl.disabled.is_empty() && !self.settings.ctrl_off.is_empty() {
                    ctl.disabled = self.settings.ctrl_off.split('|').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                }
                let analog = ctl.poll();
                let actions = std::mem::take(&mut ctl.actions);
                let moved = match (analog.steering, self.last_ctl_steer) {
                    (Some(x), Some(x0)) => (x - x0).abs() > 0.02,
                    _ => false,
                };
                if analog.steering.is_some() && (moved || self.last_ctl_steer.is_none()) {
                    self.last_ctl_steer = analog.steering;
                }
                #[cfg(windows)]
                let vr_on = self.vr.is_some();
                #[cfg(not(windows))]
                let vr_on = false;
                let needs_mouse = self.mouse_drive
                    || self.game_menu.is_some()
                    || self.chooser.is_some()
                    || self.list_kind.is_some()
                    || self.navigator.as_ref().is_some_and(|n| n.map_open())
                    || !matches!(self.view.as_str(), "driver" | "outside" | "pax");
                let hide = (moved || actions.iter().any(|a| a.1)) && !needs_mouse && !vr_on;
                if self.vr_nav_edit.is_none() && hide != self.cursor_hidden.is_some() && (hide || needs_mouse) {
                    if let Some(win) = self.window.as_ref() {
                        win.set_cursor_visible(!hide);
                        self.cursor_hidden = hide.then_some(self.cursor);
                    }
                }
                if let Some(n) = ctl.notice.take() {
                    self.service_msg = Some((n, 8.0));
                }
                // the bus's force feedback (OMSI's FF_Vib_Amp, and on a wheel its forces)
                // (in every view of the bus - the wheel went slack outside and in the
                // passenger view)
                let driving = self.player.as_ref().filter(|_| matches!(self.view.as_str(), "driver" | "outside" | "pax") && !self.paused);
                let kmh = driving.map(|p| p.vehicle.physics.velocity_kmh()).unwrap_or(0.0);
                let wheel_bump = ctl.wheel_bump(driving.and_then(|p| p.vehicle.rigid.as_ref()), kmh, dt);
                ctl.feedback(crate::controllers::FfInput {
                    on: driving.is_some(),
                    kmh,
                    lateral_accel: driving.and_then(|p| p.vehicle.rigid.as_ref()).map(|r| r.accel_body.x).unwrap_or(0.0),
                    wheel_bump,
                    wheel_bump_age: 0.0,
                    vib_amp: driving.and_then(|p| p.vehicle.var("FF_Vib_Amp")).unwrap_or(0.0),
                    vib_period: driving.and_then(|p| p.vehicle.var("FF_Vib_Period")).unwrap_or(0.0),
                    dt,
                });
                // OMSI's mouse control: the cursor's place across steers, above the middle
                // of the window is the throttle, below it the brake.
                // Steering as Omsi.exe has it (0x6f4284..0x6f447b): the whole width of the
                // window is the full lock from left to right, divided by the speed in tens
                // of km/h above 10 km/h - at 50 km/h the same hand movement turns the wheel a
                // fifth as far, which is what makes the wheel feel heavier the faster the bus
                // goes. For a second after mouse steering is switched on the wheel eases
                // towards the cursor (a half-life of the time that is left), then follows it.
                let mut analog = analog;
                // the head turned by a stick or an axis set up for it (#454), at up to
                // 120 degrees a second, in the views of the bus, on foot and flying
                if analog.look != [0.0, 0.0] && self.game_menu.is_none() && self.chooser.is_none() && !self.paused {
                    let k = LOOK_STICK_DEG_S * dt * self.settings.look_sens;
                    self.look_by(analog.look[0] * k, analog.look[1] * k);
                }
                // a gamepad's stick: a target the wheel turns towards at a hand's pace (the
                // whole lock in 1.2 s), not the wheel's place itself (#200)
                if analog.stick {
                    if let (Some(x), Some(p)) = (analog.steering, self.player.as_ref()) {
                        let target = crate::controllers::gamepad_steering(x, p.vehicle.physics.velocity_kmh() as f32);
                        let now = p.vehicle.physics.controls.steering;
                        let step = dt / 1.2;
                        analog.steering = Some(now + (target - now).clamp(-step, step));
                    }
                }
                // (in every view of the bus - driver, outside, passenger and the map camera -
                // as in OMSI, where switching the camera leaves the mouse steering on: its
                // mouse steering asks only for a player's vehicle, 0x6f4257; not on foot,
                // #516)
                let bus_view = self.mouse_steers_in_view();
                if let (true, Some(s)) = (self.mouse_drive && bus_view && !self.mouse_look && !self.input_away
                                              && self.game_menu.is_none(), self.surface.as_ref()) {
                    let (w, h) = (s.config.width as f32, s.config.height as f32);
                    if std::mem::take(&mut self.center_cursor) {
                        self.cursor = (w * 0.5, h * 0.5);
                        if let Some(win) = self.window.as_ref() {
                            let _ = win.set_cursor_position(winit::dpi::PhysicalPosition::new((w * 0.5) as f64, (h * 0.5) as f64));
                        }
                    }
                    // (the speed the divisor takes, smoothed over 0.4 s: the bus's own speed
                    // trembles by fractions of a km/h from frame to frame on its springs and
                    // tyres, and at 30 km/h the wheel twitched with it by itself)
                    let raw_kmh = self.player.as_ref().map(|p| p.vehicle.physics.velocity_kmh()).unwrap_or(0.0);
                    let k_v = 1.0 - (-dt / 0.4).exp();
                    self.mouse_kmh += (raw_kmh - self.mouse_kmh) * k_v;
                    let kmh = self.mouse_kmh;
                    let base = (crate::player::mouse_steering(self.cursor.0, w, kmh) * self.settings.mouse_sens).clamp(-1.0, 1.0);
                    // (what the mouse added at the edge, up to the full lock)
                    self.mouse_edge = self.mouse_edge.clamp((-1.0 - base).min(0.0), (1.0 - base).max(0.0));
                    let target = (base + self.mouse_edge).clamp(-1.0, 1.0);
                    // the pedals as Omsi.exe has them: from the middle of the window to its
                    // top edge the throttle, to the bottom one the brake, straight on
                    let y = (2.0 * self.cursor.1 / h.max(1.0) - 1.0).clamp(-1.0, 1.0);
                    let (pedal_t, pedal_b) = ((-y).max(0.0), y.max(0.0));
                    let (steer, fade) = &mut self.mouse_steer;
                    // (after the first second the wheel follows the cursor within ~60 ms: the
                    // cursor comes in bursts, and taken as it came the wheel moved in steps)
                    let k = if *fade > 0.0 { (-std::f32::consts::LN_2 / *fade * dt).exp() } else { (-dt / 0.06).exp() };
                    *steer = target + (*steer - target) * k;
                    let (mt, mb) = &mut self.mouse_pedals;
                    *mt = crate::player::mouse_pedal(*mt, pedal_t, k);
                    *mb = crate::player::mouse_pedal(*mb, pedal_b, k);
                    *fade = (*fade - dt).max(0.0);
                    analog.steering = Some(*steer);
                    // OMSI_TRACE_STEER=<csv>: the mouse steering frame by frame
                    if let Some(path) = omsi_cfg::env::var_os("OMSI_TRACE_STEER") {
                        use std::io::Write;
                        static TRACE: std::sync::Mutex<Option<std::fs::File>> = std::sync::Mutex::new(None);
                        let mut g = TRACE.lock().unwrap_or_else(|e| e.into_inner());
                        if g.is_none() {
                            *g = std::fs::File::create(&path).ok();
                            if let Some(f) = g.as_mut() {
                                let _ = writeln!(f, "t,dt,cursor_x,kmh,target,steer,steer_deg");
                            }
                        }
                        let deg = self.player.as_ref().map(|p| p.vehicle.physics.steer_deg).unwrap_or(0.0);
                        if let Some(f) = g.as_mut() {
                            let _ = writeln!(f, "{:.3},{:.4},{:.1},{:.2},{:.4},{:.4},{:.3}", self.clock.run_time, dt, self.cursor.0, kmh, target, self.mouse_steer.0, deg);
                        }
                    }
                    // the mouse owns the wheel (OMSI sets the curvature from it every frame):
                    // a steering key's leftover turn must not take over whenever the cursor
                    // passes the middle - the wheel jumped there; and the pedals, which Omsi.exe
                    // writes from the cursor every frame: a brake the keys held stayed on (#395)
                    if let Some(p) = self.player.as_mut() {
                        p.axes.steering = 0.0;
                        p.axes.brake = 0.0;
                        p.axes.throttle = 0.0;
                    }
                    analog.throttle = Some(self.mouse_pedals.0);
                    analog.brake = Some(self.mouse_pedals.1);
                } else if self.mouse_drive && bus_view && (self.mouse_look || self.input_away)
                    && self.game_menu.is_none() {
                    // looking round with the right button: the wheel and the pedals stay where
                    // the mouse left them, as in OMSI (they went slack until the button was let
                    // go - no quick look round while driving). With the window in the
                    // background the same, its throttle let go (`App::input_lost`): the
                    // cursor wandering over other windows steered and drove the bus.
                    analog.steering = Some(self.mouse_steer.0);
                    analog.throttle = Some(self.mouse_pedals.0);
                    analog.brake = Some(self.mouse_pedals.1);
                    if let Some(p) = self.player.as_mut() {
                        p.axes.steering = 0.0;
                    }
                }
                // the controller's view buttons are the game's, not the bus's: looking around
                // while held (`view_look_*`), and OMSI's view actions (other cameras, views)
                let mut actions = actions;
                if self.game_menu.is_none() {
                    let mut game: Vec<String> = Vec::new();
                    actions.retain(|(name, down)| {
                        let n = name.to_ascii_lowercase();
                        if let Some(k) = ["view_look_left", "view_look_right", "view_look_up", "view_look_down"].iter().position(|x| *x == n) {
                            self.pad_look[k] = *down;
                            return false;
                        }
                        if n == "gear_up" || n == "gear_down" {
                            if *down {
                                game.push(n);
                            }
                            return false;
                        }
                        if crate::input_script::is_game_action(&n) {
                            if *down {
                                game.push(n);
                            }
                            return false;
                        }
                        true
                    });
                    for n in game {
                        match n.as_str() {
                            "gear_up" => { self.shift_gear(true); }
                            "gear_down" => { self.shift_gear(false); }
                            _ => { self.game_action(&n); }
                        }
                    }
                }
                if let Some(p) = self.player.as_mut() {
                    p.axes.linear = self.settings.steering_linear;
                    p.axes.old_steering = self.settings.old_steering;
                    p.axes.red_steer_spd = self.settings.red_steer_spd;
                    p.axes.pedal_hold = self.settings.brake_hold;
                    p.analog = analog;
                    if self.game_menu.is_none() {
                        for (name, down) in actions {
                            p.action(&name, down);
                        }
                    }
                }
                // the on-screen wheel and pedals (a phone)
                self.touch_frame(dt);
                if let (Some(p), Some(r), Some(scene)) = (
                    self.player.as_mut(),
                    self.renderer.as_ref(),
                    self.scene.as_mut(),
                ) {
                    // (while the tile under the bus is being read again - the weather turned
                    // to snow and every tile came back with the winter textures - there is
                    // no ground under it: it is held where it is rather than falling through
                    // the world and being put back somewhere in the sky)
                    let ground_here = self.world.as_ref().is_none_or(|w| {
                        let at = p.vehicle.position;
                        let k = ((at.x / omsi_map::tile_size()).floor() as i32, (at.y / omsi_map::tile_size()).floor() as i32);
                        w.terrains.read().contains_key(&k) || w.surfaces.read().contains_key(&k)
                    });
                    if !self.paused && ground_here {
                        p.tick(
                            dt,
                            self.audio.as_ref(),
                            self.in_cab,
                            !matches!(self.view.as_str(), "free" | "foot"),
                        );
                        // (not in the headset: the player's own head moves there, and a head
                        // thrown about by the bus on top of it made the whole cab sway and
                        // shift before the eyes)
                        #[cfg(windows)]
                        let vr_on = self.vr.is_some();
                        #[cfg(not(windows))]
                        let vr_on = false;
                        p.move_head(dt, self.settings.head_movement && !vr_on);
                        // (a head that is doing nothing still breathes and shifts its weight:
                        // the sway goes on the head and the view while the bus waits, never
                        // into the springs above. Nothing of it while a headset or a real
                        // head tracker moves the head - that head is not a still one)
                        let idle = if vr_on || (self.settings.head_tracking && self.headtrack.is_some()) { 0.0 } else { self.settings.head_idle };
                        // (a switch under the cursor is a hand reaching for it, and a view that
                        // goes on sliding under the pointer is a view that misses what it was
                        // reaching for. Held, not reset: the camera stays where it is, which is
                        // where any camera is while the player is busy with something)
                        let reaching = idle > 0.0 && (self.hover.is_some() || self.hover_hand);
                        if !self.head_idle_hold.step(dt, reaching) {
                            p.move_head_idle(dt, idle, self.settings.head_idle_pace);
                        }
                        if let Some(w) = self.world.as_ref() {
                            crate::rail_drive::frame(p, self.traffic.as_ref().map(|t| &t.net), w, dt);
                        }
                    }
                    // a script that set the time of day (`(S.S.Time)`) moves the game's clock
                    if let Some(t) = p.vehicle.host.time_written.take() {
                        self.pending_time = Some(t);
                    }
                    // the situation's further vehicles stand and run their scripts, and the
                    // player's bus meets them
                    let mut placed_boxes = Vec::new();
                    for q in self.placed.iter_mut() {
                        if !self.paused {
                            q.vehicle.update(dt);
                        }
                        q.sync_transforms(r, scene, false);
                        let f = crate::lan::footprint_of(&q.vehicle, [2.5, 11.5, 3.0, 0.0, 0.0, 1.5]);
                        placed_boxes.push(omsi_sim::collision::Obb {
                            center: glam::DVec2::new(f.x, f.y),
                            half: glam::DVec2::new(f.width as f64 * 0.5, f.length as f64 * 0.5),
                            heading: (f.heading as f64).to_radians(),
                            z0: f.z,
                            z1: f.z + 3.0,
                            velocity: glam::DVec2::ZERO,
                            mass: 12_000.0,
                            pole: None,
                            id: -1,
                        });
                    }
                    if !self.placed.is_empty() {
                        // (the traffic writes the list afresh every frame; without it, this does)
                        if self.traffic.is_none() {
                            p.vehicle.dynamic_boxes.clear();
                        }
                        p.vehicle.dynamic_boxes.extend(placed_boxes);
                    }
                    if let Some(w) = self.world.as_ref() {
                        lay_down_poles(w, r, scene, &mut p.vehicle);
                    }
                    static EVERY: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
                    if let Some(every) = *EVERY.get_or_init(|| {
                        omsi_cfg::env::var("OMSI_DEBUG_PHYSICS")
                            .ok()
                            .and_then(|v| v.parse::<f32>().ok())
                            .filter(|v| *v > 0.0)
                    }) {
                        static LAST: std::sync::atomic::AtomicU32 =
                            std::sync::atomic::AtomicU32::new(u32::MAX);
                        let t = self.started.elapsed().as_secs_f32();
                        let bucket = (t / every) as u32;
                        if LAST.swap(bucket, std::sync::atomic::Ordering::Relaxed) != bucket {
                            log_physics(&p.vehicle, t);
                        }
                    }
                    let inside = self.in_cab;
                    p.sync_transforms(r, scene, inside);
                    // from the driver's seat the figure stays in the mirrors
                    // (from the driver's seat only the mirrors show him)
                    // (out of the seat: nobody at the wheel)
                    p.sync_driver_hands(r, scene, dt, self.settings.driver && self.on_foot.is_none(), self.view == "driver", self.settings.hands_in_cab);
                    if self.view != "free" && self.view != "foot" {
                        let key = crate::input_script::look_key_of(&self.view, Some(p.cam_choice));
                        crate::input_script::swap_view_look(&mut self.look, &mut self.view_looks, &mut self.look_view, &key);
                        if let Some(cam) = self.camera.as_ref() {
                            p.seat = glam::Vec3::from_array(self.settings.seat);
                            // head tracking: the head's turn on top of the look, its movement
                            // on top of the seat (opentrack: x right, y up, z back, in cm)
                            // (a port that cannot be had is tried again now and then, the
                            // setting stays on: turning it off here undid the switch in the
                            // menu at once)
                            if self.settings.head_tracking && self.headtrack.is_none() && self.headtrack_failed.is_none_or(|t| t.elapsed().as_secs_f32() > 5.0) {
                                self.headtrack = crate::headtrack::HeadTracker::start(self.settings.head_tracking_port);
                                self.headtrack_failed = self.headtrack.is_none().then(std::time::Instant::now);
                            }
                            let tracked = self.headtrack.as_ref().and_then(|h| h.pose()).filter(|_| self.settings.head_tracking && matches!(self.view.as_str(), "driver" | "pax"));
                            #[cfg(windows)]
                            let vr_on = self.vr.is_some();
                            #[cfg(not(windows))]
                            let vr_on = false;
                            // Camera smoothing uses frame time, not the head physics' clamped step.
                            // Physical head tracking controls the view without an added automatic turn.
                            p.steer_look = if vr_on || tracked.is_some() { 0.0 } else {
                                crate::player::steering_view_yaw(p.steer_look, p.vehicle.physics.controls.steering, dt,
                                                                 self.settings.steer_look && self.view == "driver", self.settings.steer_look_angle, self.settings.steer_look_response)
                            };
                            if let Some(t) = tracked {
                                p.seat += glam::Vec3::new(t.pos[0], -t.pos[2], t.pos[1]).clamp(glam::Vec3::splat(-60.0), glam::Vec3::splat(60.0)) / 100.0;
                            }
                            // (the outside view's field of view starts from the plain 60
                            // degrees every frame: taken from the last frame's camera, the
                            // zoom was applied on top of itself and ran off to its narrowest
                            // or widest at once)
                            let prev_cam = *cam;
                            let base = omsi_render::Camera { fov_deg: 60.0, ..*cam };
                            // what turns the bus's own camera into the picture: the head's turn,
                            // the field of view setting and the zoom (for the camera left in a
                            // switch as well as for the one taken)
                            let tracked_rot = tracked.map(|mut t| {
                                for (k, axis) in ["yaw", "pitch", "roll"].iter().enumerate() {
                                    if self.settings.head_tracking_invert.contains(axis) {
                                        t.rot[k] = -t.rot[k];
                                    }
                                }
                                t.rot
                            });
                            let fov_setting = self.settings.fov;
                            let zoom = self.view_zoom.get(&self.view).copied();
                            // The sway's own turn of the view: the driver's view only, for it is
                            // his head (and nothing at all while the sway is off or driven by a
                            // head tracker - `head_idle` is still then).
                            let idle_rot = (self.view == "driver" && !p.head_idle.is_still()).then(|| [p.head_idle.yaw, p.head_idle.pitch, p.head_idle.roll]);
                            let finish = move |c: &mut omsi_render::Camera| {
                                if let Some(r) = tracked_rot {
                                    c.yaw += r[0].clamp(-170.0, 170.0);
                                    c.pitch = (c.pitch + r[1].clamp(-80.0, 80.0)).clamp(-89.0, 89.0);
                                    c.roll += r[2].clamp(-60.0, 60.0);
                                }
                                if let Some(r) = idle_rot {
                                    c.yaw += r[0];
                                    c.pitch = (c.pitch + r[1]).clamp(-89.0, 89.0);
                                    c.roll += r[2];
                                }
                                // Settings → Field of view (0: the bus's own cameras)
                                if fov_setting >= 20.0 {
                                    c.fov_deg = fov_setting.min(120.0);
                                }
                                if let Some(z) = zoom {
                                    c.fov_deg = (c.fov_deg * z).clamp(8.0, 120.0);
                                }
                            };
                            let mut cam = p.camera_look(&self.view, &base, self.look, self.orbit);
                            finish(&mut cam);
                            // Smooth cockpit camera switch (arrow keys): the glide mixes the camera left and the one
                            // taken in the bus's own frame (smootherstep over CAM_BLEND_SECS); the bus's motion and
                            // the head go on top afterwards, so nothing of the last frame's picture is needed.
                            {
                                let inside_view = self.view == "driver";
                                let entering = std::mem::take(&mut self.cam_blend.entering);
                                let left = self
                                    .cam_blend
                                    .key
                                    .as_ref()
                                    .is_some_and(|k| k.0 == self.view && k.1 .0 != p.cam_choice.0);
                                let target = if inside_view { p.driver_local(self.look) } else { None };
                                let mut started = false;
                                if let Some(to) = target.as_ref() {
                                    if (entering || left) && crate::app::CAM_BLEND_SECS > 0.0 && self.settings.driverview_smooth {
                                        let from = if entering {
                                            // (what `driver_world` adds to every frame - the head and the seat - is
                                            // taken off the walker's eyes, and the zoom `finish` applies again off
                                            // its field of view: the first frame then is the walker's picture)
                                            let mut f = p.local_of_world(&prev_cam);
                                            let head = p.head_offset();
                                            f.pos[0] -= head.x + p.seat.x;
                                            f.pos[1] -= head.y + p.seat.y;
                                            f.pos[2] -= head.z + p.seat.z;
                                            if let Some(z) = zoom.filter(|z| *z > 0.0) {
                                                f.fov /= z;
                                            }
                                            Some(f)
                                        } else {
                                            self.cam_blend.shown.clone()
                                        };
                                        if let Some(from) = from {
                                            let d = glam::Vec3::from_array(from.pos) - glam::Vec3::from_array(to.pos);
                                            // (a far jump is another bus, not another camera of this one)
                                            if d.length() < 25.0 {
                                                self.cam_blend.from = Some(from);
                                                self.cam_blend.t = 0.0;
                                                started = true;
                                            }
                                        }
                                    }
                                }
                                self.cam_blend.key = Some((self.view.clone(), p.cam_choice));
                                let mut shown = target.clone();
                                let from_now = self.cam_blend.from.clone();
                                match (target.as_ref(), from_now.as_ref()) {
                                    (Some(to), Some(from)) => {
                                        // (the frame that starts the glide does not count, and a long frame
                                        // adds no more than a 30th of a second)
                                        if !started {
                                            self.cam_blend.t += dt.min(crate::app::CAM_BLEND_MAX_DT) / crate::app::CAM_BLEND_SECS;
                                        }
                                        if self.cam_blend.t >= 1.0 {
                                            // (the hand-over to the plain camera: the glide ends exactly on it (k = 1),
                                            // so the curve's tail is not left over to twitch; only what the two ways
                                            // of making the camera might still differ in is eased out)
                                            let mut last = p.driver_world(&crate::app::blend_local(from, to, 1.0));
                                            finish(&mut last);
                                            self.cam_blend.carry = Some(crate::app::CamCarry::between(&last, &cam));
                                            self.cam_blend.from = None;
                                        } else {
                                            let mixed = crate::app::blend_local(from, to, self.cam_blend.progress());
                                            cam = p.driver_world(&mixed);
                                            finish(&mut cam);
                                            shown = Some(mixed);
                                        }
                                    }
                                    _ => self.cam_blend.from = None,
                                }
                                self.cam_blend.shown = shown;
                                if started || !inside_view {
                                    self.cam_blend.carry = None;
                                }
                                if let Some(c) = self.cam_blend.carry.as_mut() {
                                    c.apply(&mut cam);
                                    if !c.decay(dt) {
                                        self.cam_blend.carry = None;
                                    }
                                }
                            }
                            if self.view == "outside" && self.settings.camera_collision {
                                if let Some(w) = self.world.as_ref() {
                                    cam = p.camera_clipped(cam, w, self.orbit, dt);
                                }
                            } else {
                                p.arm.reset();
                            }
                            self.camera = Some(cam);
                        }
                    } else if let Some(cam) = self.camera.as_mut() {
                        // the free camera and the view on foot follow the setting too (they
                        // stayed at 60 degrees whatever it said)
                        let base = if self.settings.fov >= 20.0 { self.settings.fov.min(120.0) } else { 60.0 };
                        cam.fov_deg = (base * self.view_zoom.get(&self.view).copied().unwrap_or(1.0)).clamp(8.0, 120.0);
                    }
                    let __th = Instant::now();
                    // (the cursor's aim into the cab: again when the cursor or the view
                    // turned, else every few frames for switches that moved under it - a ray
                    // through every cockpit mesh every frame was a tenth of the frame)
                    let key = self.camera.as_ref().map(|c| (self.cursor.0.round() as i32, self.cursor.1.round() as i32, (c.yaw * 4.0).round() as i32, (c.pitch * 4.0).round() as i32));
                    // (the cab sways with the suspension: a view that only turned waits a few frames)
                    let cursor_moved = key.map(|k| (k.0, k.1)) != self.hover_key.map(|k| (k.0, k.1));
                    if cursor_moved || (key != self.hover_key && self.total_frames % 6 == 0) || self.total_frames % 12 == 0 {
                        self.hover_key = key;
                        self.update_hover();
                    }
                    *self.profile.entry("player.hover").or_default() +=
                        __th.elapsed().as_secs_f64();
                    if let Some(a) = self.audio.as_ref() {
                        a.follow_device();
                    }
                    if let (Some(a), Some(cam)) = (self.audio.as_ref(), self.camera.as_ref()) {
                        let (reverb_time, reverb_mix) = self.world.as_ref().map(|w| w.reverb_at(cam.position)).unwrap_or((0.0, 0.0));
                        a.set_listener(omsi_audio::Listener {
                            position: cam.position.as_vec3(),
                            forward: cam.forward(),
                            right: cam.right(),
                            // (silent while paused: the engine's loops would go on)
                            // (the settings' volume: it had been 0.6 whatever the slider said)
                            master: if self.paused { 0.0 } else { self.settings.volume.clamp(0.0, 1.0) },
                            reverb_time,
                            reverb_mix,
                        });
                    }
                }
                // on foot (or the free camera) without a bus of one's own: the field of view
                // setting and the wheel's zoom, as with one - only the player's frame applied
                // them, so after removing the bus the wheel zoomed nothing (#837)
                if self.player.is_none() && matches!(self.view.as_str(), "free" | "foot") {
                    if let Some(cam) = self.camera.as_mut() {
                        let base = if self.settings.fov >= 20.0 { self.settings.fov.min(120.0) } else { 60.0 };
                        cam.fov_deg = (base * self.view_zoom.get(&self.view).copied().unwrap_or(1.0)).clamp(8.0, 120.0);
                    }
                }
                // on foot without a bus of one's own: the vehicles one placed still stand, run
                // their scripts and are drawn where they are (the player's frame did it)
                if let (None, Some(r), Some(scene)) = (self.player.as_ref(), self.renderer.as_ref(), self.scene.as_mut()) {
                    for q in self.placed.iter_mut() {
                        if !self.paused {
                            q.vehicle.update(dt);
                        }
                        q.sync_transforms(r, scene, false);
                    }
                }
                if let Some(a) = self.audio.as_ref() {
                    match self.player.as_mut() {
                        Some(p) => {
                            let inside = self.in_cab;
                            self.radio.set_map(&self.args.root, &self.args.map);
                            if let Some(m) = self.radio.update(a, &p.vehicle, inside) {
                                self.service_msg = Some((m, 6.0));
                            }
                            // (a radio whose display is a text of its script shows the station)
                            p.vehicle.radio_text = self.radio.display_text();
                            p.vehicle.radio_frequency = self.radio.frequency(p.vehicle.position.x, p.vehicle.position.y);
                        }
                        None => self.radio.stop(a),
                    }
                }
                *self.profile.entry("player").or_default() += __t.elapsed().as_secs_f64();
                let __t = Instant::now();
                self.tick_lan(dt);
                // (a stage of its own: a joining player's bus is loaded here, and that frame
                // was counted as the people's)
                *self.profile.entry("lan").or_default() += __t.elapsed().as_secs_f64();
                // the player on foot, and the other players walking about
                self.tick_on_foot(if self.paused { 0.0 } else { dt });
                self.sync_remote_walkers();
                let __t = Instant::now();
                let sight = self.camera.as_ref().zip(self.surface.as_ref())
                    .and_then(|(c, s)| self.sight_extent(c, (s.config.width, s.config.height)));
                if let (Some(h), Some(w), Some(r), Some(scene)) = (
                    self.humans.as_mut(),
                    self.world.as_ref(),
                    self.renderer.as_ref(),
                    self.scene.as_mut(),
                ) {
                    let center = self
                        .player
                        .as_ref()
                        .map(|p| p.vehicle.position)
                        .or(self.camera.as_ref().map(|c| c.position))
                        .unwrap_or(DVec3::ZERO);
                    // (the riders leave a bus the driver has walked away from)
                    if h.stop_targets.is_none() {
                        h.stop_targets = self.schedule.as_ref().map(|s| s.stop_targets());
                        h.stop_names = self.schedule.as_ref().map(|s| s.stop_names());
                        if let Some(t) = &h.stop_targets {
                            log::info!("people: {} bus stops with timetable targets", t.len());
                        }
                    }
                    h.driver_away = self.on_foot.as_ref().is_some_and(|f| {
                        let own = Some(crate::humans::BusId::Player);
                        f.seat.map(|s| s.0) != own && f.inside.map(|i| i.0) != own
                    });
                    h.density = w
                        .global
                        .passenger_density((self.clock.time / 3600.0) as f32)
                        // (OMSI's `AIPassFactor`, the passengers setting in per cent)
                        * self.settings.pax_density;
                    h.time_of_day = self.clock.time;
                    h.delay = self.duty.as_ref().map(|d| d.delay(self.clock.time)).unwrap_or(0.0);
                    self.humans_populate_t -= dt;
                    if self.humans_populate_t <= 0.0 && !self.paused {
                        self.humans_populate_t = 2.0;
                        h.populate(w, r, scene, center);
                    }
                    if let (Some(cam), Some(s)) = (self.camera.as_ref(), self.surface.as_ref()) {
                        h.eye = Some(humans::Eye::of(
                            cam,
                            s.config.width as f32 / s.config.height.max(1) as f32,
                        ).widened(sight));
                    }
                    // (the other LAN players' buses, for their riders to sit in)
                    h.set_remote_buses(self.remotes.remotes.iter().map(|(id, r)| (*id, r.vehicle())));
                    // (and the vehicles the player placed and left, with their riders)
                    h.set_placed_buses(self.placed.iter().map(|q| (q.uid, &q.vehicle)));
                    let took = h.tick(
                        if self.paused { 0.0 } else { dt },
                        w,
                        self.player.as_ref().map(|p| &p.vehicle),
                        self.traffic.as_ref(),
                        r,
                        scene,
                    );
                    // validators used: the bus's `ev_Stamper` sound
                    for bus in h.take_stamped() {
                        match bus {
                            None => {
                                if let Some(p) = self.player.as_mut() {
                                    p.vehicle.host.fired_triggers.push("ev_Stamper".into());
                                }
                            }
                            Some(id) => {
                                if let Some(c) = self.traffic.as_mut().and_then(|t| t.cars.iter_mut().find(|c| c.id == id)) {
                                    c.vehicle.host.fired_triggers.push("ev_Stamper".into());
                                }
                            }
                        }
                    }
                    if let Some(t) = self.traffic.as_mut() {
                        let (alighting, waiting) = h.stop_wishes();
                        t.set_stop_wishes(alighting, waiting);
                        for (id, stop, secs) in h.take_holds() {
                            t.hold_boarding(id, stop, secs);
                        }
                        for (id, entry, exit) in h.take_ai_requests() {
                            t.set_pax_requests(id, &entry, &exit);
                        }
                    }
                    if let Some(m) = h.take_message() {
                        self.service_msg = Some((m, 6.0));
                    }
                    if let Some(p) = self.player.as_mut() {
                        if took {
                            p.vehicle.set_var("GivenTicket", -1.0);
                        }
                        h.give_ticket = std::mem::take(&mut p.give_ticket);
                        h.give_change_all = std::mem::take(&mut p.give_change);
                        if std::mem::take(&mut p.take_change) {
                            h.take_change_tray();
                        }
                        if std::mem::take(&mut h.stop_request) {
                            // a passenger's request is the vehicle trigger Omsi.exe fires
                            // (0x62e42c), not the cab's stop button `door_haltewunsch`,
                            // whose switch and brake sounds some buses play
                            p.vehicle.trigger("int_haltewunsch");
                        }
                        h.write_pax_vars(&mut p.vehicle);
                        p.vehicle.host.humans_on_path_link = h.path_link_counts();
                        p.vehicle.host.humans_on_seat = h.seat_counts();
                        let coins: Vec<usize> = std::mem::take(&mut p.vehicle.host.change_coins);
                        h.give_change(w, r, scene, &coins);
                        h.sync_money(r, scene, &p.vehicle);
                    }
                    h.sync(r, scene, center);
                }
                *self.profile.entry("humans").or_default() += __t.elapsed().as_secs_f64();
                self.foot_after_humans();
                if let (Some(d), Some(p), Some(w), false) = (
                    self.duty.as_mut(),
                    self.player.as_mut(),
                    self.world.as_ref(),
                    self.paused,
                ) {
                    if let Some(stop) = p.html_next_stop.take() {
                        if d.skip_to(stop) {
                            let (trip, k) = d.trip_for_ibis();
                            p.ibis_to_stop(trip, k);
                        }
                    }
                    d.learn_loaded(&w.object_positions.lock());
                    if let Some((arrival, departure)) = d.update(&mut p.vehicle, self.clock.time) {
                        self.career.stop_served(arrival, departure);
                    }
                    if d.take_trip_change() && p.duty_typed {
                        let (trip, stop) = d.trip_for_ibis();
                        p.set_duty_destination(trip, stop);
                    }
                    let mut fonts = w.fonts.lock();
                    if let Err(e) = crate::schedule_paper::update_vehicle(
                        &mut p.vehicle,
                        d,
                        &mut fonts,
                    ) {
                        log::warn!("driver timetable paper: {e:#}");
                    }
                }
                if let Some(p) = self.player.as_mut() {
                    let riders = self.humans.as_ref().map(|h| h.riding()).unwrap_or(0);
                    // the engine's own variables of the bus (see `update_engine_vars`)
                    p.vehicle.host.humans_count = riders as f32;
                    p.vehicle.host.schedule_active = if self.duty.is_some() { 1.0 } else { 0.0 };
                    let crash = std::mem::take(&mut p.vehicle.last_crash);
                    // (a frame after the session was written must not start another one)
                    if !self.exiting && !self.paused {
                        self.career.tick(dt, &p.vehicle, riders);
                    }
                    if crash > 0.0 {
                        self.career.crashed(crash, p.vehicle.physics.velocity_kmh() / 3.6);
                        self.service_msg = Some((format!("Crash: {:.0} kJ", crash / 1000.0), 6.0));
                    }
                }
                if !self.paused {
                    crate::admin::guard_fall(self, dt);
                }
                self.placing_frame();
                // the host sends every edit of the map again now and then (players join)
                if self.lan.as_ref().map(|l| l.role == omsi_net::Role::Host).unwrap_or(false) {
                    self.editor_sync_t -= dt;
                    if self.editor_sync_t <= 0.0 {
                        self.editor_sync_t = 10.0;
                        self.editor_broadcast(true);
                    }
                }
                // --on-foot: the bus the start put down goes, the player stands beside it
                if self.args.on_foot && self.world.is_some() {
                    self.args.on_foot = false;
                    if self.player.is_some() {
                        self.remove_driven_vehicle();
                    } else if let Some(c) = self.camera.as_ref() {
                        // (no bus came: where the camera stands, on the ground)
                        let p = c.position;
                        let z = self.world.as_ref().and_then(|w| w.walk_height(p.x, p.y)).unwrap_or(p.z - 1.7);
                        let yaw = c.yaw as f64;
                        self.start_on_foot(glam::DVec3::new(p.x, p.y, z), yaw);
                    }
                    self.service_msg = Some(("On foot: Esc menu, Place a vehicle..., then G at its driver's door to drive it".into(), 8.0));
                }
                // Discord's status: the map, the bus, the line (every few seconds)
                #[cfg(not(target_os = "android"))]
                {
                    self.discord_t -= dt;
                    if self.discord_t <= 0.0 {
                        self.discord_t = 5.0;
                        if self.args.server.is_none()
                            && self.discord.is_none()
                            && self.settings.discord_status
                        {
                            self.discord =
                                crate::discord::Discord::start(&self.settings.discord_app_id);
                        }
                        if let Some(d) = self.discord.as_ref() {
                            let bus = self.player.as_ref().map(|p| {
                                let definition = &p.vehicle.ty.def;
                                let short = omsi_launcher_lib::vehicle_type_label(&definition.type_name, &definition.path);
                                let full = omsi_launcher_lib::display_bus_name(&format!("{} {short}", definition.manufacturer));
                                (short, full)
                            });
                            let duty = self.duty.as_ref().map(|d| (d.line.as_str(), d.tour.as_str()));
                            d.set(crate::discord::Presence::for_game(
                                self.world.as_ref().map(|w| w.global.name.as_str()),
                                bus.as_ref().map(|(short, full)| (short.as_str(), full.as_str())),
                                duty,
                                self.lan.is_some(),
                            ));
                        }
                    }
                }

                // Steam's callbacks (rich presence)
                #[cfg(steam)]
                if let Some(steam) = self.steam.as_ref() {
                    steam.client.run_callbacks();
                }
                // the plugins' frame, with the bus's scripts done
                let plugins = self.plugins.get_or_insert_with(crate::plugins::load);
                if !plugins.is_empty() && !self.paused {
                    let info = crate::plugins::game_info(self);
                    let keys = std::mem::take(&mut self.plugin_keys);
                    let plugins = self.plugins.as_mut().unwrap();
                    let mut io = crate::plugins::Io { vehicle: self.player.as_mut().map(|p| &mut p.vehicle), dt, message: None, info, commands: Vec::new(), keys };
                    plugins.frame(&mut io);
                    let commands = std::mem::take(&mut io.commands);
                    if let Some(m) = io.message {
                        self.service_msg = Some(m);
                    }
                    // what the plugins asked the game to do: lines of the game menu
                    for c in commands {
                        if let Some(k) = self.game_menu_items().iter().position(|m| m.0 == c) {
                            let was = self.game_menu;
                            self.menu_prev_pause = self.paused;
                            self.menu_choose(event_loop, k);
                            // (an action leaves the menu as it found it)
                            if self.chooser.is_none() && was.is_none() {
                                self.game_menu = None;
                            }
                        } else {
                            // (a line of the vehicle or world pages)
                            self.menu_prev_pause = self.paused;
                            self.page_action(&c);
                        }
                    }
                } else {
                    self.plugin_keys.clear();
                }
                // OMSI_WATCH_VARS=a,b: every change of those variables of the player's bus
                if let (Some(p), Ok(list)) = (self.player.as_ref(), omsi_cfg::env::var("OMSI_WATCH_VARS")) {
                    thread_local!(static LAST: std::cell::RefCell<std::collections::HashMap<String, f32>> = Default::default());
                    LAST.with(|last| {
                        let mut last = last.borrow_mut();
                        for n in list.split(',').map(str::trim).filter(|n| !n.is_empty()) {
                            let v = p.vehicle.var(n).unwrap_or(f32::NAN);
                            if last.get(n).is_none_or(|&o| o.to_bits() != v.to_bits()) {
                                log::info!("watch: {n} = {v} at {:.2} s", self.clock.time);
                                last.insert(n.to_string(), v);
                            }
                        }
                    });
                }
                if let (Some(h), Some(p)) = (self.humans.as_mut(), self.player.as_ref()) {
                    self.career.tickets = (h.tickets_sold as i32, h.ticket_cash as f64);
                    self.career.boarded = h.boarded as i32;
                    self.career.served = h.served as i32;
                    self.career.stepped_in = h.stepped_in as i32;
                    self.career.content = h.content as i32;
                    self.career.ticket_requests = h.ticket_requests as i32;
                    self.career.ticket_points = h.ticket_points as i32;
                    // the options' [no_collision_pedastrians]: nobody is knocked down
                    let hurt = if self.settings.collision_pedestrians { h.run_over(&p.vehicle) } else { 0 };
                    if hurt > 0 {
                        self.career.crashes[1] += hurt as i32;
                        self.service_msg = Some(("Pedestrian knocked down!".into(), 6.0));
                    }
                }
                // looking around and zooming work in every view, not only the free camera
                self.sync_view_look();
                if self.player.is_some() && self.view != "free" {
                    // looking around with the keyboard: Alt + I/J/K/L (the plain letters
                    // belong to the bus - L is the headlights in Inputs/keyboard.cfg)
                    let step = 60.0 * dt;
                    // Ctrl+Alt+arrows in the cab: the mirror nearest to where the driver looks
                    // turns (kept per bus in mirrors.cfg when the keys are let go)
                    // the mirror editor: an arrow held over a panel aims that panel's mirror
                    // (kept per bus like Ctrl+Alt+arrows below)
                    if let (Some(size), Some(a)) =
                        (self.mirror_hud_size(), self.mirror_hud.turning())
                    {
                        if let (Some(i), Some(p)) = (
                            self.mirror_hud.cam_under(self.hud_cursor(), size),
                            self.player.as_mut(),
                        ) {
                            let n = p.vehicle.ty.def.cameras_reflexion.len();
                            if p.mirror_offsets.len() < n {
                                p.mirror_offsets.resize(n, [0.0; 2]);
                            }
                            if p.mirror_shifts.len() < n {
                                p.mirror_shifts.resize(n, [0.0; 3]);
                            }
                            // Alt+arrows and Page Up/Down shift the mirror (0.2 m a second, at most
                            // 0.6 m across and up, a metre along), the plain arrows aim it
                            if p.mirror_fovs.len() < n {
                                p.mirror_fovs.resize(n, 0.0);
                            }
                            let alt = self.keys.contains(&KeyCode::AltLeft) || self.keys.contains(&KeyCode::AltRight);
                            let along = (a[4] as i32 - a[5] as i32) as f32;
                            let zoom = (a[7] as i32 - a[6] as i32) as f32;
                            if zoom != 0.0 {
                                // - and + narrow and widen the mirror's field of view (20° a second)
                                if let Some(f) = p.mirror_fovs.get_mut(i) {
                                    *f = (*f + 20.0 * dt * zoom).clamp(-60.0, 60.0);
                                    p.mirrors_dirty = true;
                                    let base = p.vehicle.ty.def.cameras_reflexion.get(i).map(|c| if c.fov > 1.0 { c.fov } else { 50.0 }).unwrap_or(50.0);
                                    self.service_msg = Some((format!("Mirror {}: field of view {:.0}° (the bus's {:.0}°)", i + 1, (base + *f).clamp(8.0, 110.0), base), 2.0));
                                }
                            } else if alt || along != 0.0 {
                                let metres = 0.2 * dt;
                                if let Some(s) = p.mirror_shifts.get_mut(i) {
                                    if alt {
                                        s[0] = (s[0] + metres * (a[1] as i32 - a[0] as i32) as f32).clamp(-0.6, 0.6);
                                        s[2] = (s[2] + metres * (a[2] as i32 - a[3] as i32) as f32).clamp(-0.6, 0.6);
                                    }
                                    s[1] = (s[1] + metres * along).clamp(-1.0, 1.0);
                                    p.mirrors_dirty = true;
                                    self.service_msg = Some((format!("Mirror {} shifted {:+.2} m across, {:+.2} m forward, {:+.2} m up", i + 1, s[0], s[1], s[2]), 2.0));
                                }
                            } else if let Some(o) = p.mirror_offsets.get_mut(i) {
                                let rate = 12.0 * dt;
                                o[0] = (o[0] + rate * (a[1] as i32 - a[0] as i32) as f32).clamp(-45.0, 45.0);
                                o[1] = (o[1] + rate * (a[2] as i32 - a[3] as i32) as f32).clamp(-30.0, 30.0);
                                p.mirrors_dirty = true;
                                self.service_msg = Some((format!("Mirror {}: {:+.1}° across, {:+.1}° up", i + 1, o[0], o[1]), 2.0));
                            }
                        }
                    }
                    let ctrl_alt = (self.keys.contains(&KeyCode::ControlLeft) || self.keys.contains(&KeyCode::ControlRight)) && (self.keys.contains(&KeyCode::AltLeft) || self.keys.contains(&KeyCode::AltRight));
                    let arrows = [KeyCode::ArrowLeft, KeyCode::ArrowRight, KeyCode::ArrowUp, KeyCode::ArrowDown].map(|k| self.keys.contains(&k));
                    if let (true, Some(p), Some(cam)) = (ctrl_alt && self.view == "driver" && arrows.iter().any(|a| *a), self.player.as_mut(), self.camera.as_ref()) {
                        let cams = &p.vehicle.ty.def.cameras_reflexion;
                        let f = cam.forward();
                        let best = (0..cams.len())
                            .map(|i| (i, (p.vehicle.camera_world_full(&cams[i]).0 - cam.position).as_vec3().normalize_or_zero().dot(f)))
                            .max_by(|a, b| a.1.total_cmp(&b.1))
                            .map(|(i, _)| i);
                        if let Some(i) = best {
                            if p.mirror_offsets.len() <= i {
                                p.mirror_offsets.resize(cams.len(), [0.0; 2]);
                            }
                            let o = &mut p.mirror_offsets[i];
                            let rate = 12.0 * dt;
                            o[0] = (o[0] + rate * (arrows[1] as i32 - arrows[0] as i32) as f32).clamp(-45.0, 45.0);
                            o[1] = (o[1] + rate * (arrows[2] as i32 - arrows[3] as i32) as f32).clamp(-30.0, 30.0);
                            p.mirrors_dirty = true;
                            self.service_msg = Some((format!("Mirror {}: {:+.1}° across, {:+.1}° up (Ctrl+Alt+arrows)", i + 1, o[0], o[1]), 2.0));
                        }
                    } else if let Some(p) = self.player.as_mut().filter(|p| p.mirrors_dirty) {
                        p.mirrors_dirty = false;
                        crate::settings::save_mirror_state(&p.vehicle.ty.def.path, &p.mirror_offsets, &p.mirror_shifts, &p.mirror_fovs);
                    }
                    // a controller's look buttons (Settings → Controllers: view_look_*)
                    self.look.0 += step * 1.5 * (self.pad_look[1] as i32 - self.pad_look[0] as i32) as f32;
                    self.look.1 = (self.look.1 + step * 0.7 * (self.pad_look[2] as i32 - self.pad_look[3] as i32) as f32).clamp(-85.0, 85.0);
                    // with a wheel steering, the arrow keys look around as in OMSI
                    if !ctrl_alt && self.controllers.as_ref().is_some_and(|c| c.wheel_steering()) && !self.keys.contains(&KeyCode::ControlLeft) && !self.keys.contains(&KeyCode::ControlRight) {
                        // a glance: held, the head turns (to 140 degrees at most); let go, it
                        // comes back to the road - held, it went round and round, and the
                        // other key never brought it back straight
                        let (l, r) = (self.keys.contains(&KeyCode::ArrowLeft), self.keys.contains(&KeyCode::ArrowRight));
                        if l || r {
                            self.look.0 = crate::input_script::cab_look_yaw(&self.view, self.look.0 + step * 1.5 * (r as i32 - l as i32) as f32);
                            self.arrow_glance = true;
                        } else if self.arrow_glance {
                            self.look.0 *= (-6.0 * dt).exp();
                            // (down to a hundredth of a degree before it is set to 0: at half a
                            // degree the last step was a visible snap of several pixels)
                            if self.look.0.abs() < 0.02 {
                                self.look.0 = 0.0;
                                self.arrow_glance = false;
                            }
                        }
                        if self.keys.contains(&KeyCode::ArrowUp) {
                            self.look.1 = (self.look.1 + step * 0.7).min(85.0);
                        }
                        if self.keys.contains(&KeyCode::ArrowDown) {
                            self.look.1 = (self.look.1 - step * 0.7).max(-85.0);
                        }
                    }
                    let alt = self.keys.contains(&KeyCode::AltLeft)
                        || self.keys.contains(&KeyCode::AltRight);
                    if alt && self.keys.contains(&KeyCode::KeyJ) {
                        self.look.0 -= step;
                    }
                    if alt && self.keys.contains(&KeyCode::KeyL) {
                        self.look.0 += step;
                    }
                    if alt && self.keys.contains(&KeyCode::KeyI) {
                        self.look.1 = (self.look.1 + step * 0.7).min(85.0);
                    }
                    if alt && self.keys.contains(&KeyCode::KeyK) {
                        self.look.1 = (self.look.1 - step * 0.7).max(-85.0);
                    }
                    if self.view != "outside" {
                        self.look.0 = crate::input_script::cab_look_yaw(&self.view, self.look.0);
                    }
                    // Ctrl+Shift+Page Up / Page Down held: the clock runs forwards / backwards,
                    // a quarter of an hour per second at first, faster the longer it is held
                    // (the menu's whole hours were the only way)
                    {
                        let ctrl = self.keys.contains(&KeyCode::ControlLeft) || self.keys.contains(&KeyCode::ControlRight);
                        let shift = self.keys.contains(&KeyCode::ShiftLeft) || self.keys.contains(&KeyCode::ShiftRight);
                        let dir = match (self.keys.contains(&KeyCode::PageUp), self.keys.contains(&KeyCode::PageDown)) {
                            (true, false) => 1.0,
                            (false, true) => -1.0,
                            _ => 0.0,
                        };
                        let client = self.lan.as_ref().is_some_and(|l| l.role == omsi_net::Role::Client);
                        if ctrl && shift && dir != 0.0 && !client {
                            if self.clock_hold == 0.0 && self.real_time_locked() {
                                // (says once why the clock stays)
                                self.shift_clock(dir as f64);
                            }
                            self.clock_hold += dt;
                            if !self.real_time_locked() {
                                let rate = 900.0 * (1.0 + self.clock_hold * 1.5).min(8.0);
                                self.shift_clock(dir * rate as f64 * dt as f64);
                            }
                        } else {
                            self.clock_hold = 0.0;
                        }
                    }
                    // = and - zoom inside the bus (the numpad's are door keys there), unless
                    // the player bound them to something of their own (#701)
                    let zoom_in = self.keys.contains(&KeyCode::Equal) && !self.own_keys.contains(&13);
                    let zoom_out = self.keys.contains(&KeyCode::Minus) && !self.own_keys.contains(&12);
                    if matches!(self.view.as_str(), "driver" | "pax") {
                        if zoom_in {
                            self.zoom_by(3.0 * dt);
                        }
                        if zoom_out {
                            self.zoom_by(-3.0 * dt);
                        }
                    }
                    // W/S and the wheel pull the outside camera in and out
                    if self.view == "outside" {
                        if zoom_in || self.keys.contains(&KeyCode::NumpadAdd)
                        {
                            self.orbit = (self.orbit - 12.0 * dt).max(ORBIT_MIN);
                        }
                        if zoom_out || self.keys.contains(&KeyCode::NumpadSubtract)
                        {
                            self.orbit = (self.orbit + 12.0 * dt).min(ORBIT_MAX);
                        }
                    }
                    // Home held recentres the view - unless keyboard.cfg gives it a job (the
                    // stock file makes it the ticket desk camera, which this then turned
                    // straight ahead again whenever it was switched to, #733)
                    if self.keys.contains(&KeyCode::Home) && !self.game_keys.iter().any(|b| Some(b.scan_code) == crate::keys::dik_code(KeyCode::Home)) {
                        self.look = (0.0, 0.0);
                        self.orbit = ORBIT_DEFAULT;
                        self.view_zoom.remove(&self.view);
                    }
                }
                if self.view != "free" {
                    self.ego = false;
                }
                // (the free camera flies; with no bus it is the view too - but not out of the
                // walker's eyes: on foot without a bus of one's own (started on foot, the bus
                // removed) the keys flew the camera on from where the walk had put it every
                // frame, and walking jumped about, the more so the lower the frame rate, #807)
                if let (Some(cam), true) = (
                    self.camera.as_mut(),
                    self.view == "free" || (self.player.is_none() && self.on_foot.is_none()),
                ) {
                    let mut v = Vec3::ZERO;
                    let f = cam.forward();
                    let r = cam.right();
                    if self.keys.contains(&KeyCode::KeyW) {
                        v += f;
                    }
                    if self.keys.contains(&KeyCode::KeyS) {
                        v -= f;
                    }
                    if self.keys.contains(&KeyCode::KeyD) {
                        v += r;
                    }
                    if self.keys.contains(&KeyCode::KeyA) {
                        v -= r;
                    }
                    if self.keys.contains(&KeyCode::KeyE) || self.keys.contains(&KeyCode::Space) {
                        v += Vec3::Z;
                    }
                    if self.keys.contains(&KeyCode::KeyQ) {
                        v -= Vec3::Z;
                    }
                    let boost = if self.keys.contains(&KeyCode::ShiftLeft) {
                        5.0
                    } else {
                        1.0
                    };
                    if self.ego {
                        // walking: along the ground at eye height, 1.4 m/s (running 4.5)
                        let flat = Vec3::new(v.x, v.y, 0.0).normalize_or_zero();
                        let pace = if boost > 1.0 { 4.5 } else { 1.4 };
                        cam.position += (flat * pace * dt).as_dvec3();
                        if let Some(g) = self.world.as_ref().and_then(|w| w.walk_height(cam.position.x, cam.position.y)) {
                            cam.position.z = g + 1.7;
                        }
                    } else {
                        cam.position += (v.normalize_or_zero() * self.speed * boost * dt).as_dvec3();
                    }
                    if self.keys.contains(&KeyCode::ArrowLeft) {
                        cam.yaw -= 60.0 * dt;
                    }
                    if self.keys.contains(&KeyCode::ArrowRight) {
                        cam.yaw += 60.0 * dt;
                    }
                    if self.keys.contains(&KeyCode::ArrowUp) {
                        cam.pitch = (cam.pitch + 40.0 * dt).min(89.0);
                    }
                    if self.keys.contains(&KeyCode::ArrowDown) {
                        cam.pitch = (cam.pitch - 40.0 * dt).max(-89.0);
                    }
                }
                // (the METAR sync: the report's weather, in real time)
                self.tick_metar(dt);
                if !self.paused {
                    // (the time speed: the settings', or the session's in LAN play)
                    let speed = self.time_speed();
                    self.clock.advance(dt * speed as f32);
                    // (the real-time sync: the device's date and time, whatever the speed was)
                    self.sync_real_time();
                    if let Some(t) = self.traffic.as_mut() {
                        t.time_scale = speed;
                    }
                    self.tick_weather(dt * speed as f32);
                } else if self.weather_blend.is_some() {
                    // (a preset picked in the paused menu: the change goes over in real time)
                    self.tick_weather(dt);
                }
                let daylight = omsi_sim::Daylight::compute(&self.clock, self.envir.as_ref());
                if self.lamps_on != Some(daylight.lamps_on) {
                    self.lamps_on = Some(daylight.lamps_on);
                    if let (Some(w), Some(r), Some(scene)) = (
                        self.world.as_ref(),
                        self.renderer.as_ref(),
                        self.scene.as_mut(),
                    ) {
                        w.set_lamps(r, scene, daylight.lamps_on);
                    }
                }
                // the lit windows of the houses by their [NightMapMode] timetable (once a
                // second: tiles come and go, and the hours pass)
                if self.total_frames % 60 == 0 {
                    if let (Some(w), Some(r), Some(scene)) = (
                        self.world.as_ref(),
                        self.renderer.as_ref(),
                        self.scene.as_mut(),
                    ) {
                        w.update_night_modes(r, scene, &self.clock, daylight.brightness);
                    }
                    self.follow_date();
                }
                if let Some(p) = self.player.as_mut() {
                    let lm = self.world.as_ref().and_then(|w| w.light_map_light_at(p.vehicle.position));
                    p.vehicle.set_var("Envir_Brightness", daylight.envir_brightness(lm));
                    p.vehicle.host.sun_alt = daylight.altitude_deg;
                    if let Some(w) = &self.weather {
                        apply_weather(&mut p.vehicle, w, self.wetness);
                    }
                }
                let __t = Instant::now();
                // the lamps' cones in fog and falling rain or snow, and new light pictures
                if let Some(wt) = &self.weather {
                    lights::set_cone_strength(wt.fog.0, precip_of(wt).1, daylight.night);
                }
                if let Some(r) = self.renderer.as_mut() {
                    lights::upload_corona_textures(r);
                }
                // the tile light maps around the camera, for the roads' night light
                let __ta = Instant::now();
                if let (Some(w), Some(r), Some(cam)) = (self.world.as_ref(), self.renderer.as_ref(), self.camera.as_ref()) {
                    w.update_light_map_atlas(r, cam.position);
                }
                *self.profile.entry("lights.atlas").or_default() += __ta.elapsed().as_secs_f64();
                if let (Some(w), Some(scene), Some(cam)) = (
                    self.world.as_ref(),
                    self.scene.as_mut(),
                    self.camera.as_ref(),
                ) {
                    let mut vehicles: Vec<&omsi_sim::VehicleInstance> = Vec::new();
                    if let Some(p) = self.player.as_ref() {
                        vehicles.push(&p.vehicle);
                    }
                    if let Some(t) = self.traffic.as_ref() {
                        vehicles.extend(t.cars.iter().map(|c| &c.vehicle));
                    }
                    vehicles.extend(self.remotes.remotes.values().map(|r| r.vehicle()));
                    let __tc = Instant::now();
                    lights::collect(w, scene, &daylight, cam.position, &vehicles);
                    *self.profile.entry("lights.collect").or_default() += __tc.elapsed().as_secs_f64();
                    // the object editor's pick: a magenta glow over it
                    if let Some(id) = self.editor.as_ref().and_then(|e| e.selected) {
                        let at = w.edit_objects.lock().get(&id).map(|o| o.pos);
                        let moved = w.object_edits.lock().get(&id).map(|e| e.moved).unwrap_or_default();
                        if let Some(p) = at {
                            scene.coronas.push(omsi_render::Corona {
                                position: p + moved + glam::DVec3::Z * 3.0,
                                size: 0.6,
                                color: [1.0, 0.1, 0.9],
                                brightness: 2.0,
                                ..Default::default()
                            });
                        }
                    }
                    if let Some(wt) = &self.weather {
                        let (kind, rate) = precip_of(wt);
                        self.rain.set(kind, rate);
                        // [wind] direction (deg) speed (m/s)
                        let wind = Vec3::new(
                            wt.wind.0.to_radians().sin() * wt.wind.1,
                            wt.wind.0.to_radians().cos() * wt.wind.1,
                            0.0,
                        );
                        // every bus one may ride in keeps the weather out: the own, another
                        // player's, a timetable bus - each part of it: an articulated bus's
                        // rear section is a coupled part with its own [boundingbox] (#777)
                        let boxed = crate::rain::vehicle_boxes;
                        let mut buses: Vec<(glam::DVec3, f64, [f32; 6])> = self.player.as_ref().map(|p| boxed(&p.vehicle)).unwrap_or_default();
                        buses.extend(self.remotes.remotes.values().flat_map(|rv| boxed(rv.vehicle())));
                        if let Some(t) = self.traffic.as_ref() {
                            buses.extend(t.cars.iter().filter(|c| c.is_bus() && (c.vehicle.position - cam.position).length() < 40.0).flat_map(|c| boxed(&c.vehicle)));
                        }
                        let __tr = Instant::now();
                        self.rain.tick(if self.paused { 0.0 } else { dt }, cam.position, wind, scene, &buses);
                        *self.profile.entry("lights.rain").or_default() += __tr.elapsed().as_secs_f64();
                        // wheel splashes through the puddles enhanced.wgsl paints on wet roads
                        if kind == 1 {
                            if let Some(p) = self.player.as_ref() {
                                let wheels = puddles::wheel_contacts(&p.vehicle);
                                let speed = p.vehicle.physics.velocity_kmh().abs() / 3.6;
                                let wetness = self.wetness;
                                scene.smoke.extend(self.splashes.update(
                                    dt,
                                    &wheels,
                                    speed,
                                    &|x, y| {
                                        puddles::puddle_coverage(x, y, w.wet_road_at(x, y, wetness))
                                    },
                                ));
                            }
                        }
                        // the rain heard in the street and the footsteps on the pavement
                        if let (Some(amb), Some(a)) = (self.ambience.as_mut(), self.audio.as_ref())
                        {
                            let steps = self
                                .humans
                                .as_mut()
                                .map(|h| h.take_footfalls())
                                .unwrap_or_default();
                            // what the passengers say, where they stand
                            for line in self.humans.as_mut().map(|h| h.take_voice_lines()).unwrap_or_default() {
                                if let Some(clip) = a.load_clip(&line.path) {
                                    a.play(
                                        clip,
                                        omsi_audio::mixer::VoiceParams {
                                            gain: 1.0,
                                            pitch: 1.0,
                                            looping: false,
                                            position: Some(line.position.as_vec3()),
                                            doppler: true,
                                            range: 3.0,
                                            lowpass_hz: 0.0,
                                            important: false,
                                        },
                                    );
                                }
                            }
                            let inside = self.in_cab;
                            let __tm = Instant::now();
                            amb.update(
                                a,
                                dt,
                                (kind, rate),
                                inside,
                                street_condition(wt, self.wetness),
                                cam.position,
                                &steps,
                            );
                            *self.profile.entry("lights.ambience").or_default() += __tm.elapsed().as_secs_f64();
                            if let Some(every) = debug_sound_every() {
                                static LAST: std::sync::atomic::AtomicU32 =
                                    std::sync::atomic::AtomicU32::new(u32::MAX);
                                let bucket = (self.clock.time / every as f64) as u32;
                                if LAST.swap(bucket, std::sync::atomic::Ordering::Relaxed) != bucket
                                {
                                    log::info!("sound: environment - {} (precip {kind} {rate:.2}, StreetCond {:.2}, {} voices)", amb.last, street_condition(wt, self.wetness), a.voice_count());
                                }
                            }
                        }
                    }
                }
                *self.profile.entry("lights+rain").or_default() += __t.elapsed().as_secs_f64();
                let __t = Instant::now();
                if let (Some(w), Some(r), Some(scene), Some(cam)) = (
                    self.world.as_ref(),
                    self.renderer.as_ref(),
                    self.scene.as_mut(),
                    self.camera.as_ref(),
                ) {
                    let traffic = self.traffic.as_ref();
                    let phase = |c: usize, li: usize| {
                        traffic.map(|t| t.light_vars(c, li)).unwrap_or((omsi_sim::traffic::UNLINKED_PHASE as f32, 0.0))
                    };
                    let __tb = Instant::now();
                    if let Some(p) = self.player.as_mut() {
                        w.sync_html_departures(&mut p.vehicle.host);
                    }
                    match self.schedule.as_mut() {
                        Some(s) => s.update_boards(
                            w,
                            traffic,
                            self.duty.as_ref(),
                            self.player
                                .as_ref()
                                .and_then(|p| p.vehicle.host.hof.as_deref()),
                            &self.clock,
                        ),
                        None => w.timetable_boards.lock().clock = Some(self.clock.clone()),
                    }
                    *self.profile.entry("scripted.boards").or_default() += __tb.elapsed().as_secs_f64();
                    w.update_scripted(
                        r,
                        scene,
                        dt,
                        cam.position,
                        daylight.brightness,
                        &phase,
                        self.audio.as_ref(),
                        self.in_cab,
                    );
                }
                *self.profile.entry("scripted").or_default() += __t.elapsed().as_secs_f64();
                // (the game menu's lines, for the interface below)
                let menu_lines = if self.game_menu.is_some() { self.game_menu_items() } else { Vec::new() };
                let vr_nav_display = self.vr_nav_display();
                let vr_active = self.vr_active();
                // the interface over the picture
                // (the pages of an open settings window)
                let menu_tabs = match self.list_kind.as_ref() {
                    Some(k) if self.chooser.is_some() => crate::game_lists::page_titles(self, k),
                    _ => None,
                };
                if let (true, Some(r), Some(scene)) = (
                    self.world.is_some(),
                    self.renderer.as_ref(),
                    self.scene.as_mut(),
                ) {
                    // No block of text over the picture (the clock, the bus, the trip and the
                    // key reminder were the old HUD in OMSI's bitmap font; the navigator shows
                    // the trip, the launcher the keys): only what the driver has to act on,
                    // in the interface font, top left.
                    let mut lines: Vec<String> = Vec::new();
                    // why the bus is not moving, whenever the throttle is pressed and nothing
                    // happens: the things a driver checks first
                    if let Some(p) = self.player.as_ref() {
                        lines.extend(standing_reasons(&p.vehicle, &|a| crate::diagnostics::rebound_key(&p.bindings, a)));
                    }
                    // what is under the cursor, in the player's language (the scripts only
                    // know internal, mostly German names)
                    let names = describe::names(&self.args.root, &self.settings.language);
                    // next to the cursor (`ui`), when the setting asks for it
                    let tooltip = self.hover.as_ref().map(|h| names.control(h));
                    // the object editor's keys, while it is on (one quiet line)
                    if self.editor.is_some() {
                        lines.push("Object editor: click picks · drag moves · wheel turns (Shift lifts) · Del · C copy · V variant · Backspace undo · Ctrl+S save · Esc".into());
                    }
                    if let Some(d) = self.duty.as_ref().filter(|d| d.trip_done()) {
                        lines.push(match d.trips.get(d.trip_index + 1) {
                            Some(next) => format!(
                                "End of the trip. Next: {} to {}, from {} at {} (it starts by itself a minute before)",
                                if next.line.trim().is_empty() { "service trip".to_string() } else { format!("line {}", next.line) },
                                next.terminus.strip_prefix(&format!("{} ", next.line)).unwrap_or(&next.terminus),
                                next.stops.first().map(|s| s.name.trim()).unwrap_or("?"),
                                crate::schedule::hhmm(next.departure)
                            ),
                            None => "End of the duty: the tour's last trip is done".into(),
                        });
                    }
                    if let Some((msg, left)) = self.service_msg.as_mut() {
                        *left -= dt;
                        if *left > 0.0 {
                            lines.push(msg.clone());
                        }
                    }
                    self.service_msg = self.service_msg.take().filter(|(_, l)| *l > 0.0);
                    for n in self.notices.iter_mut() {
                        n.left -= dt;
                    }
                    self.notices.retain(|n| n.left > 0.0);
                    if let Some(lan) = self.lan.as_ref() {
                        lines.extend(lan::hud_lines(lan, &self.remotes, self.player.as_ref()));
                        lines.extend(self.voice.as_ref().and_then(|v| v.hud_line()));
                    }
                    if let Some(h) = self.humans.as_ref() {
                        if let Some(hint) = h.hint() {
                            lines.push(hint);
                        } else if let Some((name, value)) = &h.request {
                            lines.push(format!("Passenger wants: {name}  {value:.2}"));
                        }
                        if let Some((paid, value)) = h.paid {
                            lines.push(format!(
                                "paid: {paid:.2}  (change {:.2})",
                                (paid - value).max(0.0)
                            ));
                        }
                        if let Some(owed) = h.change_due {
                            lines.push(format!("Change due: {owed:.2}"));
                        }
                    }
                    let __t = Instant::now();
                    // (the frame's overlays start empty; the notes are the interface's, in
                    // Roboto - OMSI's bitmap font HUD is the start menu's and the offscreen
                    // pictures' only)
                    scene.overlays.clear();
                    let notes = lines;
                    let hud = self
                        .surface
                        .as_ref()
                        .map(|s| {
                            self.settings
                                .hud_viewport((s.config.width, s.config.height))
                        })
                        .unwrap_or([0.0, 0.0, 1.0, 1.0]);
                    if let (Some(nav), Some(p), Some(_)) = (
                        self.navigator.as_mut(),
                        self.player.as_ref(),
                        self.surface.as_ref(),
                    ) {
                        let old_enabled = nav.enabled;
                        let old_opacity = nav.opacity;
                        nav.cockpit_display = vr_active;
                        if vr_active {
                            nav.enabled = vr_nav_display.is_some_and(|d| d.placement.enabled);
                            nav.opacity = vr_nav_display.map(|d| d.placement.opacity).unwrap_or(0.95);
                        }
                        if let Some(w) = self.world.as_ref() {
                            nav.start_map(w.clone());
                        }
                        // stops beyond the loaded tiles: their places from the navigator's map
                        if let (Some(places), Some(d)) = (nav.places(), self.duty.as_mut()) {
                            if !self.duty_places {
                                self.duty_places = true;
                                d.learn_places(places);
                            }
                        }
                        let (line, terminus, stops, trip) = navigator::duty_parts(self.duty.as_ref());
                        match (trip, self.schedule.as_ref(), self.traffic.as_ref(), self.world.as_ref()) {
                            (Some((key, name)), Some(sch), _, _) if nav.map_net().is_some() => {
                                if nav.wants_route(&key, 0) {
                                    let lanes = sch.trip_route_in(nav.map_net().unwrap(), &name);
                                    let g = nav.global_version + (1 << 40);
                                    nav.set_route(&key, lanes, true, g);
                                }
                            }
                            (Some((key, name)), Some(sch), Some(t), Some(w)) => {
                                if nav.wants_route(&key, t.lanes_generation) {
                                    let (lanes, complete) = sch.trip_route(w, t, &name);
                                    nav.set_route(&key, lanes, complete, t.lanes_generation);
                                }
                            }
                            _ => nav.clear_route(),
                        }
                        let (outside_temp, inside_temp) = vehicle_temperatures(p);
                        // (on foot the map follows the walker, not the bus left standing)
                        let (at, heading) = match self.on_foot.as_ref() {
                            Some(f) => (f.pos, f.heading),
                            None => (p.vehicle.position, p.vehicle.heading),
                        };
                        let frame = navigator::NavFrame {
                            traffic: self.traffic.as_ref(),
                            bus: at,
                            heading,
                            speed_kmh: p.vehicle.physics.velocity_kmh(),
                            outside_temp,
                            inside_temp,
                            line,
                            terminus,
                            stops,
                            delay: self.duty.as_ref().map(|_| p.vehicle.host.tt_delay as f64),
                            passengers: self.humans.as_ref().map(|h| h.riding()),
                            stop_requested: navigator::stop_requested(&p.vehicle),
                            time: self.clock.time,
                            weekday: self.clock.weekday(),
                            language: &self.settings.language,
                            screen: if vr_active {
                                (1440.0, 1440.0)
                            } else {
                                (hud[2], hud[3])
                            },
                            ui_scale: if vr_active {
                                1.0
                            } else {
                                self.settings.ui_scale
                            },
                            follow_window: if vr_active {
                                true
                            } else {
                                self.settings.ui_scale_window
                            },
                            dt,
                        };
                        let __tn = Instant::now();
                        nav.frame_at(r, scene, &frame, hud[0]);
                        nav.enabled = old_enabled;
                        nav.opacity = old_opacity;
                        *self.profile.entry("hud.navigator").or_default() += __tn.elapsed().as_secs_f64();
                        // OMSI 2's dynamic route arrows over the junctions ahead
                        if nav.arrows {
                            if let Some(w) = self.world.as_ref() {
                                let spots = nav.arrow_spots(self.traffic.as_ref().map(|t| &t.net), 350.0, &|id| w.object_positions.lock().get(&id).map(|p| (p.0, p.1[0])));
                                self.route_arrows.tick(dt, w, r, scene, &spots);
                            }
                        }
                    }
                    if let (Some(ui), Some(s)) = (self.ui.as_mut(), self.surface.as_ref()) {
                        let scale = self.window.as_ref().map(|w| w.scale_factor() as f32).unwrap_or(1.0);
                        let (w, h) = (hud[2], hud[3]);
                        self.remotes.chat.disabled = !self.settings.chat;
                        let chat = (self.lan.is_some() && self.settings.chat).then(|| ui::ChatView {
                            lines: &self.remotes.chat.lines,
                            typing: self.remotes.chat.typing.as_deref(),
                            error: self.remotes.chat.error(),
                        });
                        ui.chat.hidden = self.remotes.chat.hidden;
                        let mut tags = if self.settings.name_tags {
                            let voice = self.voice.as_ref();
                            let speaks = |name: &str, id: u32| voice.is_some_and(|v| v.speaks(name, id));
                            let rig = (self.settings.triple.enabled
                                && !self.settings.vr_requested())
                            .then(|| {
                                self.settings.triple.zoomed(
                                    s.config.width,
                                    s.config.height,
                                    self.view_zoom.get(&self.view).copied().unwrap_or(1.0),
                                )
                            });
                            self.camera
                                .as_ref()
                                .map(|c| {
                                    lan::name_tags(
                                        &self.remotes,
                                        c,
                                        s.config.width as f32,
                                        h,
                                        rig.as_ref(),
                                        &speaks,
                                    )
                                })
                                .unwrap_or_default()
                        } else {
                            Vec::new()
                        };
                        for (pos, _, _, _) in &mut tags {
                            pos.0 -= hud[0];
                        }
                        // the vehicle chooser shows its vehicles in the menu's place (the menu
                        // scrolls a long list)
                        // the name of the cab's switch under the cursor, unless the interface
                        // covers the cab there (it read like a line of the menu over it)
                        let (cx, cy) = self.cursor;
                        let map_open = self.navigator.as_ref().is_some_and(|n| n.map_open());
                        let covered = self.game_menu.is_some()
                            || self.vr_nav_edit.is_some()
                            || self.chooser.is_some()
                            || ui.chat.hovered
                            || map_open
                            || (!vr_active && self.navigator.as_ref().is_some_and(|n| n.over_panel(cx, cy)));
                        let dropdown = self.dropdown.as_ref().filter(|_| self.chooser.is_some()).map(|d| ui::DropdownView {
                            row: d.row,
                            items: d.items.iter().map(|x| x.0.as_str()).collect(),
                            sel: d.sel,
                            top: d.top,
                            current: d.current,
                        });
                        let chooser_list = self.admin_list.as_ref().unwrap_or(&self.vehicle_list);
                        let (chooser_items, chooser_sel): (Vec<(&str, &str)>, Option<usize>) = match self.chooser {
                            Some(sel) => {
                                let items = chooser_list.iter().map(|(name, path)| (path.as_str(), name.as_str())).collect();
                                (items, Some(sel))
                            }
                            None => (Vec::new(), None),
                        };
                        // (the game menu's greyed-out lines: the timetable needs an active route)
                        let menu_disabled: &[&str] = &[];
                        let (menu_kind, menu_head, menu_preview) = crate::game_lists::menu_extras(self.list_kind.as_ref(), self.admin_list.as_deref(), chooser_sel, self.schedule.as_ref(), self.clock.time);
                        let frame = ui::Frame {
                            scale,
                            ui_scale: ui::size_factor(h, scale, self.settings.ui_scale, self.settings.ui_scale_window),
                            opacity: ui::backdrop(self.settings.ui_opacity),
                            width: w,
                            height: h,
                            cursor: (self.cursor.0 - hud[0], self.cursor.1),
                            vr: {
                                #[cfg(windows)] { self.vr.is_some() }
                                #[cfg(not(windows))] { false }
                            },
                            tooltip: tooltip.filter(|_| self.settings.tooltips && !self.dragging && !covered && self.game_menu.is_none()),
                            // (switched off: none, `Settings::notes`; nor over the city map,
                            // whose header they covered once they stood on the timetable's line)
                            notes: if self.settings.notes && !map_open && self.game_menu.is_none() { &notes } else { &[] },
                            fps: self.settings.show_fps.then_some(self.fps),
                            paused: self.paused,
                            menu: match chooser_sel {
                                Some(k) => Some((k, &chooser_items[..])),
                                None => self.game_menu.map(|k| (k, &menu_lines[..])),
                            },
                            menu_disabled,
                            menu_kind,
                            menu_head,
                            menu_preview,
                            pane_first: self.pane_scroll.filter(|p| Some(p.0) == chooser_sel).map(|p| p.1),
                            menu_tabs,
                            dropdown,
                            menu_kbd: self.menu_kbd,
                            menu_top: self.menu_top,
                            // (not over the city map, which has the stops and their times: it
                            // covered the map's zoom and close buttons)
                            timetable: (self.timetable && !map_open).then(|| timetable_rows(self.duty.as_ref(), self.player.as_ref().map(|p| p.vehicle.host.tt_delay as f64))).flatten(),
                            info: self.info_bar.then(|| info_line(&self.clock, self.player.as_ref(), self.duty.as_ref(), self.humans.as_ref().map(|h| h.riding()))),
                            tutorial: self.tutorial.as_ref().filter(|t| !t.hidden && self.game_menu.is_none()).and_then(|t| t.page().map(|p| (p.title.as_str(), p.text.as_str(), p.image.as_deref(), t.at, t.pages.len()))),
                            chat,
                            tags,
                            notices: &self.notices,
                            notice_anchor: self.navigator.as_ref().and_then(|n| n.screen_rect()),
                        };
                        ui.draw_at(r, scene, &frame, dt, hud[0]);
                    }
                    *self.profile.entry("hud").or_default() += __t.elapsed().as_secs_f64();
                }

                let mut lighting = match self.weather.as_ref() {
                    Some(w) => {
                        self.wetness = road_wetness(precip_of(w).1, dt as f64, self.wetness);
                        weather_lighting(
                            &daylight,
                            w,
                            self.cloud_drift,
                            self.wetness,
                            self.settings.shadows,
                        )
                    }
                    None => lights::lighting_from(&daylight, 50000.0),
                };
                lighting.wetness = omsi_cfg::env::var("OMSI_WETNESS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(self.wetness);
                lighting.inside = match self.inside_remote.and_then(|id| self.remotes.remotes.get(&id)) {
                    // (in another player's bus: its box is the one the camera is in)
                    Some(rv) => rv.vehicle().ty.def.bounding_box.map(|bb| (rv.vehicle().position, rv.vehicle().heading, bb)),
                    None => self.player.as_ref().and_then(|p| {
                        p.vehicle
                            .ty
                            .def
                            .bounding_box
                            .map(|bb| (p.vehicle.position, p.vehicle.heading, bb))
                    }),
                };
                let puddle_surface = lighting.inside.and_then(|(o, _, _)| self.world.as_ref().and_then(|w| w.puddle_surface(o)));
                lighting.puddle_ground = puddle_surface.map(|(h, _)| h);
                lighting.puddle_normal = puddle_surface.map_or(glam::Vec3::Z, |(_, n)| n);
                let puddle_vehicle = self.inside_remote.and_then(|id| self.remotes.remotes.get(&id)).map(|rv| rv.vehicle())
                    .or_else(|| self.player.as_ref().map(|p| &p.vehicle));
                lighting.puddle_parts = puddle_vehicle.into_iter().flat_map(|v| &v.trailers)
                    .filter_map(|t| t.ty.def.bounding_box.map(|bb| (t.position, t.heading, bb))).take(3).collect();
                lighting.detail = self.settings.detail_textures;
                lighting.glass_wind = self.player.as_ref().map(|p| crate::lights::vehicle_velocity(&p.vehicle)).unwrap_or_default();
                // an LED panel's dots burn this much above their own colour (16 levels,
                // see `Settings::led_glow`); the panel's picture and its mask are held at
                // this mip level at most (`Settings::led_mips`)
                lighting.led_glow = self.settings.led_glow as f32 * 0.25;
                lighting.led_mips = self.settings.led_mips;
                let mut finish = false;
                let mut reconfigure = false;
                let shot = self.shot.take();
                if let Some(s) = self.surface.as_ref() {
                    let (w, h) = (s.config.width, s.config.height);
                    self.touch_prepare(w, h);
                }
                if let (Some(s), Some(r), Some(scene), Some(cam), Some(win)) = (
                    self.surface.as_ref(),
                    self.renderer.as_mut(),
                    self.scene.as_mut(),
                    self.camera.as_ref(),
                    self.window.as_ref(),
                ) {
                    // `shot <file>` from the input script: the scene the window is showing,
                    // from its camera and lighting, into a PNG - the only way to look at
                    // what an automated window run draws (also when the window is hidden,
                    // so it does not depend on a frame being acquired)
                    if let Some(path) = shot {
                        match r.render_to_image(
                            scene,
                            s.config.width,
                            s.config.height,
                            cam,
                            &lighting,
                        ) {
                            Ok(mut px) => match {
                                // (with the on-screen controls, when there are)
                                if let Some(over) = self.touch.picture(r, s.config.width, s.config.height) {
                                    crate::touch::composite(&mut px, &over);
                                }
                                image::save_buffer(
                                    &path,
                                    &px,
                                    s.config.width,
                                    s.config.height,
                                    image::ColorType::Rgba8,
                                ) } {
                                Ok(()) => log::info!(
                                    "input script: window picture written to {}",
                                    path.display()
                                ),
                                Err(e) => log::warn!(
                                    "input script: {} could not be written: {e}",
                                    path.display()
                                ),
                            },
                            Err(e) => log::warn!(
                                "input script: the window picture could not be rendered: {e}"
                            ),
                        }
                    }
                    // A window that is hidden (another app covers it, another Space) gets
                    // no frames on macOS. OMSI_RENDER_OCCLUDED=1 draws them into a texture
                    // of the window's size anyway and waits for the GPU as a present would,
                    // so frame times can be measured with the window out of sight.
                    let __t = Instant::now();
                    // OMSI_HIDE_WINDOW=from,to: treat the window as hidden between these
                    // seconds of the session (the frame is acquired and dropped unshown), to
                    // check the hidden-window path without covering the window by hand
                    let hide_test = omsi_cfg::env::var("OMSI_HIDE_WINDOW").ok().and_then(|v| {
                        let mut it = v.split(',').filter_map(|x| x.trim().parse::<f32>().ok());
                        Some((it.next()?, it.next()?))
                    });
                    let hidden_now = hide_test
                        .map(|(a, b)| (a..b).contains(&self.started.elapsed().as_secs_f32()))
                        .unwrap_or(false);
                    let acquired = match s.surface.get_current_texture() {
                        wgpu::CurrentSurfaceTexture::Success(_)
                        | wgpu::CurrentSurfaceTexture::Suboptimal(_)
                        if hidden_now =>
                            {
                                wgpu::CurrentSurfaceTexture::Occluded
                            }
                        other => other,
                    };
                    let (frame, stand_in) = match acquired {
                        wgpu::CurrentSurfaceTexture::Success(frame)
                        | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => (Some(frame), None),
                        wgpu::CurrentSurfaceTexture::Occluded
                        if omsi_cfg::env::var_os("OMSI_RENDER_OCCLUDED").is_some() =>
                            {
                                let (w, h) = (s.config.width, s.config.height);
                                if self
                                    .stand_in
                                    .as_ref()
                                    .map(|t| (t.width(), t.height()) != (w, h))
                                    .unwrap_or(true)
                                {
                                    self.stand_in =
                                        Some(r.device.create_texture(&wgpu::TextureDescriptor {
                                            label: Some("hidden window"),
                                            size: wgpu::Extent3d {
                                                width: w,
                                                height: h,
                                                depth_or_array_layers: 1,
                                            },
                                            mip_level_count: 1,
                                            sample_count: 1,
                                            dimension: wgpu::TextureDimension::D2,
                                            format: r.format(),
                                            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                                            view_formats: &[],
                                        }));
                                }
                                (
                                    None,
                                    self.stand_in
                                        .as_ref()
                                        .map(|t| t.create_view(&Default::default())),
                                )
                            }
                        wgpu::CurrentSurfaceTexture::Outdated
                        | wgpu::CurrentSurfaceTexture::Lost => {
                            reconfigure = true;
                            (None, None)
                        }
                        _ => (None, None),
                    };
                    *self.profile.entry("acquire").or_default() += __t.elapsed().as_secs_f64();
                    if frame.is_none() {
                        self.hidden_frames += 1;
                    }
                    let shown_nothing = frame.is_none() && stand_in.is_none();
                    let view = frame
                        .as_ref()
                        .map(|f| f.texture.create_view(&Default::default()))
                        .or(stand_in);
                    if let Some(view) = view {
                        let __t = Instant::now();
                        // One mirror a turn, in turn, at most MIRROR_RATE pictures a second in
                        // all: a mirror costs half the main picture's CPU time, and at 140 fps
                        // five mirrors were each redrawn 28 times a second, a small picture
                        // that nobody can tell from 15.
                        // Every mirror at least MIRROR_MIN_HZ, though: with eight of them (the
                        // Procity) at 25 fps each was redrawn three times a second, and the
                        // street jerked past in them - up to two a frame then (each costs a
                        // few milliseconds of the frame).
                        if let (Some(p), Some(w)) = (self.player.as_ref(), self.world.as_ref()) {
                            self.mirror_hud.set_aspects(w.mirror_aspect.lock().clone());
                            self.mirror_hud.sync(p, self.settings.mirror_hud);
                        }
                        if self.settings.mirror_size == 0 {
                            self.mirror_budget = 0.0;
                            self.mirrors_seen = 0;
                        } else if self.settings.mirror_refresh == "off" {
                            self.mirror_budget = 0.0;
                            if let (Some(w), Some(p)) = (self.world.as_ref(), self.player.as_ref()) {
                                let since = match &self.frozen_mirrors {
                                    Some(m) if m.bus == p.uid => m.since,
                                    _ => -1.0,
                                };
                                let next = since.max(0.0) + raw_dt.min(0.1);
                                // (and while the driver turns a mirror, so it can be aimed)
                                if since < 0.0 || (since < MIRROR_FREEZE_REDRAW && next >= MIRROR_FREEZE_REDRAW) || p.mirrors_dirty {
                                    self.mirrors_seen = render_mirrors(r, scene, w, p, &lighting, None, None);
                                }
                                self.frozen_mirrors = Some(FrozenMirrors { bus: p.uid, since: next });
                            }
                        } else {
                            let mirrors = self.player.as_ref().map(|p| p.vehicle.ty.def.cameras_reflexion.len()).unwrap_or(0);
                            #[cfg(windows)]
                            let vr_active = self.vr.is_some();
                            #[cfg(not(windows))]
                            let vr_active = false;
                            let rate = {
                                if vr_active {
                                    // Preserve the user's total redraw budget. A negative
                                    // value explicitly requests every mirror each frame.
                                    omsi_cfg::env::var("OMSI_OPENXR_MIRROR_RATE")
                                        .ok()
                                        .and_then(|s| s.parse::<f32>().ok())
                                        .filter(|rate| rate.is_finite() && *rate >= -1.0)
                                        .unwrap_or(self.settings.vr_mirror_rate)
                                } else {
                                    let max_hz = if self.settings.mirror_refresh == "full" { MIRROR_MAX_HZ_FULL } else { MIRROR_MAX_HZ_ECO };
                                    MIRROR_RATE.max(mirrors as f32 * MIRROR_MIN_HZ).min(max_hz * self.mirrors_seen.max(1) as f32)
                                }
                            };
                            // The desktop camera does not follow the headset. Culling by
                            // its frustum can leave a mirror visible in VR uninitialised
                            // (black). Refresh all bus mirrors in VR, still taking turns
                            // within the configured budget; keep desktop visibility culling.
                            let mirror_view = if vr_active
                                || self.settings.triple.enabled
                                || (self.mirror_hud.active() && self.in_cab)
                            {
                                None
                            } else {
                                Some((*cam, s.config.width as f32 / s.config.height.max(1) as f32))
                            };
                            // (in the cab, and from outside too while the bus is near: its
                            // mirrors are seen from the pavement and stood frozen)
                            let near = self.player.as_ref().zip(self.camera.as_ref()).is_some_and(|(p, c)| (p.vehicle.position - c.position).length() < 12.0);
                            let draw_limit = if vr_active {
                                if self.in_cab || near {
                                    vr_mirror_updates(&mut self.mirror_budget, raw_dt, rate, mirrors)
                                } else {
                                    self.mirror_budget = 0.0;
                                    0
                                }
                            } else {
                                self.mirror_budget = (self.mirror_budget + raw_dt.min(0.1) * rate).min(2.5);
                                self.mirrors_seen.clamp(1, 2)
                            };
                            let mut drawn = 0;
                            if vr_active && draw_limit > 0 && draw_limit == mirrors {
                                // Prepare the cameras and textures only once when all
                                // mirrors are due, including the Every frame mode.
                                if let (Some(w), Some(p)) = (self.world.as_ref(), self.player.as_ref()) {
                                    self.mirror_turn = self.mirror_turn.wrapping_add(draw_limit);
                                    self.mirrors_seen = render_mirrors(r, scene, w, p, &lighting, None, mirror_view);
                                    drawn = draw_limit;
                                }
                            }
                            while (self.in_cab || near) && drawn < (if vr_active { draw_limit } else { self.mirrors_seen.clamp(1, 2) }) && (vr_active || self.mirror_budget >= 1.0) {
                                let (Some(w), Some(p)) = (self.world.as_ref(), self.player.as_ref()) else { break };
                                if !vr_active {
                                    self.mirror_budget -= 1.0;
                                }
                                drawn += 1;
                                self.mirror_turn = self.mirror_turn.wrapping_add(1);
                                self.mirrors_seen = render_mirrors(
                                    r,
                                    scene,
                                    w,
                                    p,
                                    &lighting,
                                    Some(self.mirror_turn),
                                    mirror_view,
                                );
                            }
                        }
                        *self.profile.entry("mirrors").or_default() += __t.elapsed().as_secs_f64();
                        let __t = Instant::now();
                        #[cfg(windows)]
                        let mut mirrored = false;
                        #[cfg(not(windows))]
                        let mirrored = false;
                        #[cfg(windows)]
                        if let Some(vr) = self.vr.as_mut() {
                            let menu_range = self.ui.as_ref().map(|u| u.menu_overlay_range.clone()).unwrap_or(0..0);
                            let cursor_overlay = self.ui.as_ref().and_then(|u| u.vr_cursor_overlay).filter(|_| self.vr_nav_edit.is_none());
                            let tooltip_overlay = self.ui.as_ref().and_then(|u| u.vr_tooltip_overlay).filter(|_| self.vr_nav_edit.is_none());
                            match vr.render(
                                r,
                                scene,
                                cam,
                                &lighting,
                                &view,
                                (s.config.width, s.config.height),
                                menu_range,
                                cursor_overlay,
                                tooltip_overlay,
                                self.cursor,
                                self.player.as_ref().map(|p| (p.vehicle.position, p.vehicle.body_rotation())),
                                vr_nav_display.filter(|d| d.placement.enabled).and_then(|d| {
                                    self.navigator.as_ref().and_then(|n| n.panel_overlay).map(|index| (index, d))
                                }),
                                self.player.as_ref().map(|p| p.uid),
                                self.settings.vr_head_smoothing_ms,
                                !self.mouse_drive,
                                self.vr_zoom_active,
                            ) {
                                Ok(visible) => mirrored = visible,
                                Err(e) => {
                                    log::error!("OpenXR rendering stopped: {e:#}");
                                    self.vr = None;
                                }
                            }
                        }
                        if self.in_cab {
                            if let Some(w) = self.world.as_ref() {
                                self.mirror_hud.ensure_frame(r, scene);
                                let hud = self
                                    .settings
                                    .hud_viewport((s.config.width, s.config.height));
                                let start = scene.overlays.len();
                                self.mirror_hud.push(
                                    scene,
                                    w,
                                    hud[2],
                                    hud[3],
                                    (self.cursor.0 - hud[0], self.cursor.1),
                                );
                                crate::ui::shift_overlays(scene, start, hud[0]);
                            }
                            // (the editor's keys, on screen as long as it is on)
                            if self.mirror_hud.editing() && self.service_msg.is_none() {
                                self.service_msg = Some((crate::mirror_hud::HINT.into(), 2.0));
                            }
                        }
                        if !mirrored
                            && self.settings.triple.enabled
                            && !self.settings.vr_requested()
                        {
                            let rig = self.settings.triple.zoomed(
                                s.config.width,
                                s.config.height,
                                self.view_zoom.get(&self.view).copied().unwrap_or(1.0),
                            );
                            r.render_triple(
                                scene,
                                &view,
                                s.config.width,
                                s.config.height,
                                cam,
                                &lighting,
                                &rig,
                            );
                        } else if !mirrored {
                            r.render(
                                scene,
                                &view,
                                s.config.width,
                                s.config.height,
                                cam,
                                &lighting,
                            );
                        }
                        // the on-screen controls over the picture (a phone)
                        self.touch.render(r, &view, s.config.width, s.config.height);
                        *self.profile.entry("render").or_default() += __t.elapsed().as_secs_f64();
                        if omsi_cfg::env::var_os("OMSI_PROFILE_GPU").is_some() {
                            // wait for the GPU here, so that its time shows as a stage of its own
                            let __t = Instant::now();
                            let _ = omsi_render::wait_gpu(&r.device, None);
                            *self.profile.entry("gpu").or_default() += __t.elapsed().as_secs_f64();
                        }
                        let __t = Instant::now();
                        match frame {
                            Some(frame) => {
                                // (without V-sync max_fps paces the frames: waiting for the compositor's frame callback cost a missed refresh each slow frame)
                                if self.settings.vsync {
                                    win.pre_present_notify();
                                }
                                frame.present();
                            }
                            None => {
                                let _ = omsi_render::wait_gpu(&r.device, None);
                            }
                        }
                        *self.profile.entry("present").or_default() += __t.elapsed().as_secs_f64();
                    } else {
                        // Nothing to draw into (a hidden window): the simulation goes on at
                        // a display's pace instead of spinning a core a thousand times a
                        // second. What it uploaded (traffic instances, streamed tiles,
                        // people, the navigator) waits in wgpu's staging buffers until the
                        // next submit, so submit nothing to let them go: without it a hidden
                        // window on Ahlheim grew by 100 MB a second (5.6 GB after 55 s).
                        let __t = Instant::now();
                        r.queue.submit(std::iter::empty::<wgpu::CommandBuffer>());
                        let _ = r.device.poll(wgpu::PollType::Poll);
                        *self.profile.entry("present").or_default() += __t.elapsed().as_secs_f64();
                        if let Some(rest) =
                            std::time::Duration::from_millis(16).checked_sub(now.elapsed())
                        {
                            std::thread::sleep(rest);
                        }
                    }
                    // max_fps (the original's [maxFPS]; OMSI_MAX_FPS for a test): the rest of
                    // the frame's time is slept, not spun, so a limit gives the CPU back
                    // (and keeps a laptop cool enough not to slow itself down)
                    let max_fps = omsi_cfg::env::var("OMSI_MAX_FPS")
                        .ok()
                        .and_then(|v| v.parse::<u32>().ok())
                        .unwrap_or(self.settings.max_fps);
                    // 0 = the screen's refresh rate: frames the screen never shows only heat the
                    // machine (with V-sync off and no limit an M4 drew 300 frames a second in the
                    // depot and ran hot); 1000 and more = no limit at all
                    let max_fps = if max_fps == 0 {
                        self.window.as_ref().and_then(|w| w.current_monitor()).and_then(|m| m.refresh_rate_millihertz()).map(|mhz| (mhz as f64 / 1000.0).round() as u32).filter(|r| *r >= 30).unwrap_or(120)
                    } else if max_fps >= 1000 {
                        0
                    } else {
                        max_fps
                    };
                    // a frame not shown (the window minimised or out of sight): 30 frames a
                    // second keep the simulation and the sound going; more is only heat
                    // (OMSI_RENDER_OCCLUDED, which draws them anyway, keeps its pace)
                    let max_fps = if shown_nothing { if max_fps == 0 { 30 } else { max_fps.min(30) } } else { max_fps };
                    #[cfg(windows)]
                    let vr_active = self.vr.is_some();
                    #[cfg(not(windows))]
                    let vr_active = false;
                    if max_fps > 0 && !vr_active {
                        let __t = Instant::now();
                        if let Some(rest) = std::time::Duration::from_secs_f64(1.0 / max_fps as f64)
                            .checked_sub(now.elapsed())
                        {
                            std::thread::sleep(rest);
                        }
                        *self.profile.entry("limiter").or_default() += __t.elapsed().as_secs_f64();
                    }
                    self.frames += 1;
                    let profiling = omsi_cfg::env::var_os("OMSI_PROFILE").is_some();
                    if profiling
                        && self.cpu_mark.is_none()
                        && self.started.elapsed().as_secs_f32() > 15.0
                    {
                        self.cpu_mark =
                            process_cpu_seconds().map(|c| (c, Instant::now(), self.total_frames));
                    }
                    if let (Some(limit), false) = (self.args.exit_after, self.exiting) {
                        if self.started.elapsed().as_secs_f32() > limit {
                            self.exiting = true;
                            log::info!("exit after {limit} s: {} frames total ({} with the window hidden{}), {:.1} fps average, {} frames over 50 ms, worst {:.0} ms", self.total_frames, self.hidden_frames, if omsi_cfg::env::var_os("OMSI_RENDER_OCCLUDED").is_some() { ", drawn off-screen" } else { ", not drawn" }, self.total_frames as f32 / self.started.elapsed().as_secs_f32(), self.spikes, self.worst_ms);
                            if let (Some(st), Some(w)) =
                                (self.streamer.as_ref(), self.world.as_ref())
                            {
                                log::info!("tile streaming: {} tiles loaded now, {} loaded and {} unloaded in all, {:.1} s preparing on the worker, slowest upload {:.0} ms, streaming over 16 ms in {} frames (worst {:.0} ms); {} objects + {} trees, {} rows, {} attached ({} without parent), {} unresolved", w.loaded_tiles().len(), st.loaded_total, st.unloaded_total, st.prepare_secs, st.worst_upload_ms, st.slow_frames, st.worst_frame_ms, st.stats.objects, st.stats.trees, st.stats.rows, st.stats.attached, st.stats.unattached, st.stats.failed_objects);
                                st.stats.log_ground();
                            }
                            if omsi_cfg::env::var_os("OMSI_PROFILE").is_some() {
                                let n = self.total_frames.max(1) as f64;
                                for (k, v) in &self.profile {
                                    log::info!("profile {k:10}: {:.1} ms/frame", v / n * 1000.0);
                                }
                                if let Some(h) = self.humans.as_ref() {
                                    log::info!(
                                        "profile people: {} ({})",
                                        h.people.len(),
                                        h.summary()
                                    );
                                }
                                for (k, v) in r.stats.borrow().iter() {
                                    log::info!(
                                        "profile render.{k:10}: {:.2} ms/frame",
                                        v / n * 1000.0
                                    );
                                }
                                for (k, v) in r.counts.borrow().iter() {
                                    log::info!("profile count {k}: {:.0} a frame", v / n);
                                }
                                for (pass, ms, frames) in r.gpu_pass_times() {
                                    log::info!("profile gpu pass {pass:12}: {ms:.2} ms ({frames} frames measured)");
                                }
                                if let (Some((c0, t0, f0)), Some(c1)) =
                                    (self.cpu_mark, process_cpu_seconds())
                                {
                                    let frames = self.total_frames.saturating_sub(f0).max(1) as f64;
                                    log::info!("profile: since 15 s {:.1} ms wall and {:.1} ms CPU (all threads) per frame, {:.1} cores busy", t0.elapsed().as_secs_f64() / frames * 1000.0, (c1 - c0) / frames * 1000.0, (c1 - c0) / t0.elapsed().as_secs_f64().max(1e-3));
                                }
                                let (sw, sh) = r.scene_size(s.config.width, s.config.height);
                                log::info!(
                                    "profile: window {}x{}, scene drawn at {sw}x{sh}, {}x MSAA",
                                    s.config.width,
                                    s.config.height,
                                    r.options.msaa
                                );
                            }
                            finish = true;
                            crate::platform::exit(event_loop);
                        }
                    }
                    self.total_frames += 1;
                    if self.fps_t.elapsed().as_secs_f32() >= 1.0 {
                        if omsi_cfg::env::var_os("OMSI_PROFILE").is_some() {
                            let secs = self.fps_t.elapsed().as_secs_f32();
                            log::info!("profile interval: {:.1} fps over {secs:.2} s", self.frames as f32 / secs);
                        }
                        let speed = self
                            .player
                            .as_ref()
                            .map(|p| format!(" - {:.0} km/h", p.vehicle.physics.velocity_kmh()))
                            .unwrap_or_default();
                        self.fps = self.frames as f32;
                        win.set_title(&format!(
                            "openOMSI - {} fps{speed} - {:.0},{:.0},{:.0} yaw {:.0}",
                            self.frames,
                            cam.position.x,
                            cam.position.y,
                            cam.position.z,
                            cam.yaw.rem_euclid(360.0)
                        ));
                        self.frames = 0;
                        self.fps_t = Instant::now();
                    }
                    win.request_redraw();
                }
                if reconfigure {
                    // the drawable went away under us (display change, lost surface)
                    if let (Some(s), Some(r), Some(win)) = (
                        self.surface.as_mut(),
                        self.renderer.as_ref(),
                        self.window.as_ref(),
                    ) {
                        let size = win.inner_size();
                        s.resize(r, size.width, size.height);
                    }
                }
                if finish {
                    self.finish_session();
                }
            }
            _ => {}
        }
    }

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device_id: winit::event::DeviceId,
        event: DeviceEvent,
    ) {
        if matches!(&event, DeviceEvent::Added | DeviceEvent::Removed) {
            if let Some(controllers) = self.controllers.as_ref() {
                controllers.refresh_devices();
            }
        }
        if let DeviceEvent::MouseMotion { delta } = event {
            if self.vr_nav_edit.is_some() {
                if self.window_focused { self.vr_nav_drag(delta.0 as f32, delta.1 as f32); }
                return;
            }
            // (in a view of the bus the cursor's own way turns it: move_cursor)
            if self.mouse_look {
                if !self.cursor_looks() {
                    if self.view == "outside" {
                        // F3 chase orbits at its own gain, not the head's.
                        self.sync_view_look();
                        let (y, p) = crate::input_script::chase_orbit_step(
                            self.look.0,
                            self.look.1,
                            delta.0 as f32,
                            delta.1 as f32,
                        );
                        self.look.0 = y;
                        self.look.1 = p;
                    } else {
                        let k = 0.15 * self.settings.look_sens;
                        self.look_by(delta.0 as f32 * k, delta.1 as f32 * k);
                    }
                }
            } else if self.mouse_drive && self.game_menu.is_none() {
                self.mouse_past_edge(delta.0 as f32);
            }
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        crate::game_lists::flush_settings(false);
        if self.mouse_edge != 0.0 && !self.mouse_drive {
            self.mouse_edge = 0.0;
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// The only user event: a quit signal arrived (see quit.rs).
    fn user_event(&mut self, event_loop: &ActiveEventLoop, _event: ()) {
        if let Some(sig) = quit::requested() {
            log::info!("{} received: ending the session", quit::signal_name(sig));
            self.finish_session();
            crate::platform::exit(event_loop);
        }
    }

    /// Every way out ends here (Escape, the window's close button, Cmd+Q, --exit-after, a
    /// quit signal): the session is written and the LAN peers hear that we left, before
    /// anything else is torn down (Cmd+Q ends the process without returning from the loop).
    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        crate::game_lists::flush_settings(true);
        self.finish_session();
        if let Some(lan) = self.lan.take() {
            // dropping the session says goodbye (BYE) to the host or the players
            drop(lan);
            log::info!("LAN: left the session");
        }
        // the tunnel's cloudflared and the WebSocket gateway go with the game (kept in a
        // static, which Rust never drops: cloudflared outlived every session, holding the
        // port and a public tunnel open)
        crate::lan::close_public_gateway();
        // and the launcher's LAN status file goes (Cmd+Q never returns to main's guard)
        drop(lan::StatusFileGuard);
        log::info!("game ends");
    }
}

impl App {
    /// The window's size in pixels, while the mirror panels can be worked (in the cab, no menu).
    pub(crate) fn mirror_hud_size(&self) -> Option<(f32, f32)> {
        if !self.in_cab || self.game_menu.is_some() || !self.mirror_hud.editing() {
            return None;
        }
        Some(self.hud_size())
    }

    /// The mouse wheel (or a pinch of two fingers): `amount` notches, up positive.
    pub(crate) fn wheel(&mut self, amount: f32) {
        if self.vr_nav_edit.is_some() { self.vr_nav_scroll(amount); return; }
        // over a mirror panel the wheel resizes it (Shift: wider or narrower)
        if let Some(size) = self.mirror_hud_size() {
            let shift =
                self.keys.contains(&KeyCode::ShiftLeft) || self.keys.contains(&KeyCode::ShiftRight);
            if self
                .mirror_hud
                .wheel(amount, shift, self.hud_cursor(), size)
            {
                return;
            }
        }
        // the object editor: the wheel turns (Shift: lifts) the object
        if self.game_menu.is_none() && self.editor_wheel(amount) {
            return;
        }
        // placing a vehicle: the wheel turns it
        if self.placing.is_some() && self.game_menu.is_none() {
            self.placing_wheel(amount);
            return;
        }
        // the game menu and its lists scroll with the wheel
        if self.game_menu.is_some() {
            self.menu_wheel(amount);
            return;
        }
        // the city map takes the wheel while it is open
        if let Some(n) = self.navigator.as_mut().filter(|n| n.map_open()) {
            n.map_wheel(amount, self.cursor.0, self.cursor.1);
            return;
        }
        // the wheel over the chat (or while typing) scrolls its history
        if let Some(ui) = self.ui.as_mut() {
            if self.lan.is_some() && (ui.chat.hovered || lan::chat_open(&self.remotes)) {
                ui.chat.wheel(self.remotes.chat.lines.len(), amount);
                return;
            }
        }
        // The wheel over a cockpit switch turns it: the same <event>_drag the
        // original fires while the mouse is dragged, with the notch as the
        // movement. Knobs, the sun blind and the ignition key are far easier to
        // set that way than by holding the button down and moving the mouse.
        if self.hover.is_some() && self.view != "free" {
            let ray = self.camera.as_ref().zip(self.surface.as_ref())
                .map(|(cam, s)| self.cockpit_cursor_ray(cam, (s.config.width, s.config.height)));
            if let (Some(p), Some((o, d, spread))) = (
                self.player.as_mut(),
                ray,
            ) {
                p.occlude_controls = self.view == "outside";
                if p.pick(o, d, spread).is_some() {
                    // a notch is worth a good push of the mouse: the scripts divide
                    // the movement by 10 (the ignition key), 200 (the parking brake)
                    // or 500 (the driver's window), so a few pixels would do nothing
                    p.wheel(o, d, spread, -amount * 40.0);
                    return;
                }
            }
        }
        let ctrl = self.keys.contains(&KeyCode::ControlLeft) || self.keys.contains(&KeyCode::ControlRight);
        if self.view == "outside" && self.player.is_some() && ctrl {
            // Ctrl+wheel: the outside camera stays where it is and narrows its field of view
            // (a telephoto; OMSI's own zoom there only moves the camera, as the wheel does)
            self.zoom_by(amount);
        } else if self.view == "outside" && self.player.is_some() {
            self.orbit = (self.orbit - amount * 1.5).clamp(ORBIT_MIN, ORBIT_MAX);
        } else if matches!(self.view.as_str(), "driver" | "pax") && self.player.is_some() {
            // inside the bus the wheel zooms, as in OMSI (the camera itself stays in the seat)
            self.zoom_by(amount);
        } else if matches!(self.view.as_str(), "free" | "foot") && !ctrl {
            // the free camera and on foot: the wheel zooms too (Ctrl+wheel moves the free
            // camera on, as the wheel alone did)
            self.zoom_by(amount);
        } else if let Some(cam) = self.camera.as_mut() {
            let f = cam.forward();
            cam.position += (f * amount * 4.0).as_dvec3();
        }
    }

    /// The left mouse button (or a finger's tap) where the cursor is.
    pub(crate) fn left_button(&mut self, event_loop: &ActiveEventLoop, pressed: bool) {
        if let Some(edit) = self.vr_nav_edit.as_mut() { edit.moving = pressed; return; }
        // a mirror panel is dragged with the left button (a release always ends a drag)
        if let Some(size) = self
            .mirror_hud_size()
            .or_else(|| (!pressed).then(|| self.hud_size()))
        {
            if self.mirror_hud.press(pressed, self.hud_cursor(), size) {
                return;
            }
        }
        let state = if pressed { ElementState::Pressed } else { ElementState::Released };
        // placing a vehicle: a click sets it down
        if self.placing.is_some() && self.game_menu.is_none() {
            if state == ElementState::Pressed {
                self.placing_click();
            }
            return;
        }
        // the game menu takes the clicks while it is open
        if self.game_menu.is_some() {
            if state == ElementState::Pressed {
                // (a tap or a click: only what is under the finger or the mouse is lit)
                self.menu_kbd = false;
            }
            // Releasing the mouse button finishes scrollbar dragging.
            if state == ElementState::Released {
                self.menu_drag = None;
                self.dd_scroll_drag = None;
                self.pane_scroll_drag = None;
                if self.menu_scroll_drag {
                    self.menu_scroll_drag = false;
                    self.menu_top = self.menu_top.map(f32::round);
                }
                return;
            }

            // an open drop-down takes the click: an entry is chosen, anywhere else closes it
            if self.dropdown.is_some() {
                let inside = |r: &[f32; 4]| self.cursor.0 >= r[0] && self.cursor.0 <= r[2] && self.cursor.1 >= r[1] && self.cursor.1 <= r[3];
                let hit = self.ui.as_ref().and_then(|u| u.dd_rects.iter().position(|r| inside(r)).map(|i| i + u.dd_top));
                // its scroll bar is dragged (a press on the track beside the thumb takes the
                // thumb there by its middle); before, the press closed the list (#794)
                if let Some((track, thumb)) = self.ui.as_ref().and_then(|u| u.dd_scroll).filter(|_| hit.is_none()) {
                    let bar = [thumb[0], track[1], thumb[2], track[3]];
                    if inside(&bar) {
                        let grab = if inside(&thumb) { self.cursor.1 - thumb[1] } else { (thumb[3] - thumb[1]) * 0.5 };
                        self.dd_scroll_drag = Some(grab);
                        self.drag_dropdown(self.cursor.1);
                        return;
                    }
                }
                match hit {
                    Some(i) => self.dropdown_pick(i),
                    None => self.dropdown = None,
                }
                return;
            }

            // Pressing the mouse button on the scrollbar thumb starts dragging.
            if state == ElementState::Pressed {
                if let Some(thumb) = self
                    .ui
                    .as_ref()
                    .and_then(|u| u.menu_scroll_thumb)
                {
                    if self.cursor.0 >= thumb[0]
                        && self.cursor.0 <= thumb[2]
                        && self.cursor.1 >= thumb[1]
                        && self.cursor.1 <= thumb[3]
                    {
                        self.menu_scroll_drag = true;
                        return;
                    }
                }

                // The sidebar of a settings window: a page, or the way back.
                if self.chooser.is_some() {
                    let side = self.ui.as_ref().and_then(|u| {
                        u.menu_side.iter().position(|r| {
                            self.cursor.0 >= r[0]
                                && self.cursor.0 <= r[2]
                                && self.cursor.1 >= r[1]
                                && self.cursor.1 <= r[3]
                        })
                    });
                    if let Some(i) = side {
                        self.settings_side_click(i);
                        return;
                    }
                }

                // The timetable beside a line's tours: a stop to start from, or the button.
                if self.chooser.is_some() {
                    // its scroll bar is dragged (a press on the track beside the thumb takes
                    // the thumb there by its middle)
                    if let Some((track, thumb, _, _)) = self.ui.as_ref().and_then(|u| u.menu_pane_scroll) {
                        let inside = |r: &[f32; 4]| self.cursor.0 >= r[0] && self.cursor.0 <= r[2] && self.cursor.1 >= r[1] && self.cursor.1 <= r[3];
                        if inside(&[thumb[0], track[1], thumb[2], track[3]]) {
                            let grab = if inside(&thumb) { self.cursor.1 - thumb[1] } else { (thumb[3] - thumb[1]) * 0.5 };
                            self.pane_scroll_drag = Some(grab);
                            self.drag_pane(self.cursor.1);
                            return;
                        }
                    }
                    let pane = self.ui.as_ref().and_then(|u| {
                        let inside = |r: &[f32; 4]| self.cursor.0 >= r[0] && self.cursor.0 <= r[2] && self.cursor.1 >= r[1] && self.cursor.1 <= r[3];
                        if u.menu_pane_go.as_ref().is_some_and(inside) {
                            return Some(usize::MAX);
                        }
                        if let Some(j) = u.menu_time.iter().position(inside) {
                            return Some(usize::MAX - 1 - j);
                        }
                        u.menu_pane.iter().position(inside).map(|i| i + u.menu_pane_start)
                    });
                    if let Some(i) = pane {
                        self.tour_pane_click(i);
                        return;
                    }
                }

                // Otherwise check whether a menu row was clicked.
                let hit = self.ui.as_ref().and_then(|u| {
                    u.menu_rects.iter().position(|r| {
                        self.cursor.0 >= r[0]
                            && self.cursor.0 <= r[2]
                            && self.cursor.1 >= r[1]
                            && self.cursor.1 <= r[3]
                    })
                });

                if let Some(row) = hit {
                    // (a click on a slider or a stepper sets the value there)
                    let k = row
                        + self
                        .ui
                        .as_ref()
                        .map(|u| u.menu_start)
                        .unwrap_or(0);
                    let ctl = self.ui.as_ref().and_then(|u| u.menu_ctl.get(row).copied().flatten());

                    // (a greyed-out line cannot be clicked)
                    if self.menu_item_off(k) {
                        return;
                    }

                    if let Some(c) = ctl {
                        if self.chooser.is_some() && self.cursor.0 >= c[0] && self.cursor.0 <= c[2] {
                            let fx = ((self.cursor.0 - c[0]) / (c[2] - c[0]).max(1.0)).clamp(0.0, 1.0);
                            self.chooser = Some(k);
                            // (a slider is held: it follows the cursor till the button is let go)
                            if self.list_click(k, fx) {
                                self.menu_drag = Some(k);
                            }
                            return;
                        }
                    }

                    if self.chooser.is_none() {
                        self.game_menu = Some(k);
                    }

                    // (a click on a tour shows its stops: the trip starts with the button)
                    if matches!(self.list_kind, Some(crate::game_lists::ListKind::Tours(..))) && crate::game_lists::tour_at(self, k).is_some() {
                        self.chooser = Some(k);
                        if let Some(crate::game_lists::ListKind::Tours(line, _)) = self.list_kind.clone() {
                            self.list_kind = Some(crate::game_lists::ListKind::Tours(line, None));
                        }
                        return;
                    }

                    // on the arrows round a line's value: one step down or up; elsewhere on
                    // the line as Enter
                    let arrows = self.ui.as_ref().and_then(|u| u.menu_arrows.get(row).copied().flatten());
                    match arrows {
                        Some([from, to, _]) if self.cursor.0 >= from && self.cursor.0 < to => self.chooser_adjust(k, "-"),
                        Some([_, _, plus]) if self.cursor.0 >= plus => self.chooser_adjust(k, "+"),
                        _ => self.menu_choose(event_loop, k),
                    }
                }
            }

            return;
        }
        self.on_left(state == ElementState::Pressed)
    }
}

/// OMSI's timetable window: the current trip's stops with their times, the ones served
/// greyed, the next one marked.
fn timetable_rows(duty: Option<&crate::schedule::PlayerDuty>, delay: Option<f64>) -> Option<(String, Vec<(String, String, u8)>)> {
    let d = duty?;
    let trip = d.trips.get(d.trip_index)?;
    let hm = |t: f64| format!("{:02}:{:02}", ((t / 3600.0) as i64).rem_euclid(24), ((t % 3600.0) / 60.0) as i64);
    let delay = delay.unwrap_or(0.0);
    let title = format!(
        "{} › {}   {}{}:{:02}   ({}/{})",
        if trip.line.trim().is_empty() { d.line.trim() } else { trip.line.trim() },
        trip.terminus.trim(),
        if delay < 0.0 { "−" } else { "+" },
        (delay.abs() / 60.0) as i64,
        (delay.abs() % 60.0) as i64,
        d.trip_index + 1,
        d.trips.len()
    );
    // (as a driver's paper timetable: the departure, the arrival at the last stop; a stop
    // with a wait shows both)
    let last = trip.stops.iter().rposition(|s| s.stops);
    let mut rows: Vec<(String, String, u8)> = trip
        .stops
        .iter()
        .enumerate()
        .filter(|(_, s)| s.stops)
        .map(|(k, s)| {
            let time = if Some(k) == last { hm(s.arr) } else if s.dep - s.arr >= 60.0 { format!("{}-{}", hm(s.arr), &hm(s.dep)[3..]) } else { hm(s.dep) };
            (s.name.trim().to_string(), time, if k < d.next_stop { 0 } else if k == d.next_stop { 1 } else { 2 })
        })
        .collect();
    // the trip after this one
    if let Some(next) = d.trips.get(d.trip_index + 1) {
        let name = format!("› {} {}", if next.line.trim().is_empty() { d.line.trim() } else { next.line.trim() }, next.terminus.trim());
        rows.push((name, hm(next.departure), 0));
    }
    Some((title, rows))
}

/// The outside air from the weather and the cabin air the vehicle scripts/engine maintain.
/// OMSI exposes both to every bus as Weather_Temperature and Cabinair_Temp.
pub(crate) fn vehicle_temperatures(p: &Player) -> (f32, f32) {
    let outside = p.vehicle.host.temperature;
    let inside = p
        .vehicle
        .var("Cabinair_Temp")
        .filter(|v| v.is_finite())
        .unwrap_or_else(|| outside.clamp(18.0, 25.0));
    (outside, inside)
}

/// OMSI's information bar: the time, the speed, temperatures, the passengers aboard, and the
/// trip with its next stop and delay.
fn info_line(clock: &omsi_sim::SimClock, player: Option<&Player>, duty: Option<&crate::schedule::PlayerDuty>, passengers: Option<usize>) -> String {
    let t = clock.time;
    let mut parts = vec![format!("{:02}:{:02}:{:02}", ((t / 3600.0) as i64).rem_euclid(24), ((t % 3600.0) / 60.0) as i64, (t % 60.0) as i64)];
    if let Some(p) = player {
        parts.push(format!("{:.0} km/h", p.vehicle.physics.velocity_kmh().abs()));
        let (outside, inside) = vehicle_temperatures(p);
        parts.push(format!("EXT {:.0} °C / INT {:.0} °C", outside, inside));
        // the tank as the bus's script says it (OMSI's RL_TankContent: tank_percent)
        if let Some(tank) = p.vehicle.var("tank_percent").filter(|v| v.is_finite()) {
            parts.push(format!("tank {:.0} %", (tank * 100.0).round()));
        }
        // how many are aboard right now (None: the passengers are switched off for this
        // drive, so there is nothing to count)
        if let Some(n) = passengers {
            parts.push(passengers_aboard(n));
        }
        if let Some(d) = duty {
            if let Some(trip) = d.trips.get(d.trip_index) {
                let line = if trip.line.trim().is_empty() { d.line.trim() } else { trip.line.trim() };
                parts.push(format!("{line} › {}", trip.terminus.trim()));
                if let Some(s) = trip.stops.get(d.next_stop) {
                    parts.push(format!("next: {}", s.name.trim()));
                }
                let delay = p.vehicle.host.tt_delay;
                parts.push(format!("{}{}:{:02}", if delay < 0.0 { "−" } else { "+" }, (delay.abs() / 60.0) as i64, (delay.abs() % 60.0) as i64));
            }
        }
    }
    parts.join("   ·   ")
}

/// `n` with the word for a passenger in the interface's language (singular for one; both
/// words are keys of the tables - the whole line is too much of a sentence to translate).
fn passengers_aboard(n: usize) -> String {
    format!("{n} {}", omsi_ui::tr(if n == 1 { "Passenger" } else { "Passengers" }))
}

#[cfg(test)]
mod governor_tests {
    use super::render_scale_step;

    #[test]
    fn cpu_stutters_do_not_reduce_picture_quality() {
        assert!(render_scale_step(35.0, 0.1) > 0.0);
        assert!(render_scale_step(35.0, 0.6) < 0.0);
        assert!(render_scale_step(60.0, 0.6) > 0.0);
    }
}

#[cfg(test)]
mod info_tests {
    use super::passengers_aboard;

    /// The count stands before the word, which is singular for one passenger (in the
    /// tables' language; without a lookup the English key is drawn as it is).
    #[test]
    fn one_passenger_is_written_in_the_singular() {
        assert_eq!(passengers_aboard(0), "0 Passengers");
        assert_eq!(passengers_aboard(1), "1 Passenger");
        assert_eq!(passengers_aboard(23), "23 Passengers");
    }
}

#[cfg(test)]
mod vr_mirror_tests {
    use super::vr_mirror_updates;

    #[test]
    fn every_frame_updates_all_mirrors_even_at_low_game_fps() {
        let mut budget = 0.75;
        for dt in [1.0 / 90.0, 1.0 / 30.0, 0.5] {
            assert_eq!(vr_mirror_updates(&mut budget, dt, -1.0, 8), 8);
            assert_eq!(budget, 0.0);
        }
    }

    #[test]
    fn a_high_budget_is_not_limited_to_two_mirrors_per_frame() {
        let mut budget = 0.0;
        assert_eq!(vr_mirror_updates(&mut budget, 1.0 / 60.0, 240.0, 4), 4);
        assert_eq!(vr_mirror_updates(&mut budget, 0.5, 360.0, 4), 4);
        assert!(budget <= 0.5);
    }

    #[test]
    fn fractional_credit_preserves_the_selected_total_rate() {
        for fps in [30, 60, 90] {
            let mut budget = 0.0;
            let updates: usize = (0..fps * 10).map(|_| vr_mirror_updates(&mut budget, 1.0 / fps as f32, 16.0, 4)).sum();
            assert!((159..=160).contains(&updates), "fps={fps}: {updates}");
        }
    }

    #[test]
    fn off_and_no_mirrors_discard_old_credit() {
        let mut budget = 2.5;
        assert_eq!(vr_mirror_updates(&mut budget, 0.1, 0.0, 4), 0);
        assert_eq!(budget, 0.0);
        assert_eq!(vr_mirror_updates(&mut budget, 0.1, -1.0, 0), 0);
        assert_eq!(vr_mirror_updates(&mut budget, 0.1, 360.0, 0), 0);
        assert_eq!(budget, 0.0);
    }
}
