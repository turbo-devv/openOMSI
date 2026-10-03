//! `OMSI_INPUT`: scripted keyboard, mouse and camera input for window runs, and its key names.

use super::*;


/// Global actions a controller button should send to the game instead of to the bus script.
pub(crate) fn is_game_action(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("view_")
        || matches!(
            name.as_str(),
            "sim_pause" | "screenshot" | "quicksave" | "toggel_mouse_ctrl" | "toggel_ctrler"
        )
}


/// How far (m) a click reaches a page (`[htmltexture]`) on a scenery object.
const HTML_OBJECT_REACH: f32 = 4.0;

impl App {
    /// Save the personnel file and the session summary (once: every caller ends the game,
    /// and the frames the loop still runs before it stops count no more time).
    pub(crate) fn finish_session(&mut self) {
        crate::game_lists::flush_settings(true);
        self.exiting = true;
        // (the tiles loaded on the way added to what the map lacks)
        if let Some(w) = self.world.clone() {
            let mut none = None;
            crate::app::report_missing_content(&w, &mut none);
        }
        // PluginFinalize, as OMSI calls it on the way out
        if let Some(mut p) = self.plugins.take() {
            p.finalize();
        }
        if self.player.is_none() || self.career.seconds <= 0.0 {
            return;
        }
        // the situation to continue next time (OMSI writes it when a map is left; once, as
        // the rest: `career.seconds` is zero after the first time)
        self.save_last_situation();
        if self.career.path.is_some() {
            if let Err(e) = self.career.save() {
                log::warn!("writing the personnel file: {e}");
            }
        }
        let bus = self.args.bus.clone().unwrap_or_default();
        if let Err(e) = self.career.write_session(
            &self.args.map,
            &bus,
            self.args.line.as_deref(),
            self.args.tour.as_deref(),
        ) {
            log::warn!("writing the session summary: {e}");
        }
        self.career.seconds = 0.0;
    }

    /// A key of the window, or of an `OMSI_INPUT` script.
    pub(crate) fn on_key(&mut self, event_loop: &ActiveEventLoop, code: KeyCode, pressed: bool, repeat: bool) {
        if self.vr_nav_edit.is_some() {
            if !pressed { self.keys.remove(&code); }
            if matches!(code, KeyCode::ControlLeft | KeyCode::ControlRight | KeyCode::ShiftLeft | KeyCode::ShiftRight) && pressed {
                self.keys.insert(code);
            }
            if pressed && !repeat {
                match code {
                    KeyCode::Escape | KeyCode::Enter => self.finish_vr_nav_edit(),
                    KeyCode::KeyR => self.vr_nav_adjust("reset", 1.0),
                    _ => {}
                }
            }
            return;
        }
        // The mirror panels (see mirror_hud.rs): Ctrl+M shows or hides them, Ctrl+Shift+M
        // starts and ends their editor; in the editor Insert, Delete, C and Esc are its keys.
        if self.in_cab && self.game_menu.is_none() && self.player.is_some() {
            let ctrl = self.keys.contains(&KeyCode::ControlLeft) || self.keys.contains(&KeyCode::ControlRight);
            let shift = self.keys.contains(&KeyCode::ShiftLeft) || self.keys.contains(&KeyCode::ShiftRight);
            if pressed && !repeat && code == KeyCode::KeyM && ctrl {
                if let Some(p) = self.player.as_ref() {
                    let msg = if shift { self.mirror_hud.toggle_edit(p) } else { self.mirror_hud.toggle(p) };
                    self.service_msg = Some((msg, if shift { 6.0 } else { 3.0 }));
                }
                return;
            }
            // (in the editor the arrows aim the mirror under the cursor; see the frame)
            if matches!(code, KeyCode::ArrowLeft | KeyCode::ArrowRight | KeyCode::ArrowUp | KeyCode::ArrowDown | KeyCode::PageUp | KeyCode::PageDown | KeyCode::Minus | KeyCode::Equal | KeyCode::NumpadAdd | KeyCode::NumpadSubtract) && self.mirror_hud.arrow(code, pressed) {
                return;
            }
            // R puts the mirror under the cursor back as the bus has it, Shift+R every mirror
            if self.mirror_hud.editing() && code == KeyCode::KeyR {
                if pressed && !repeat {
                    let size = self.hud_size();
                    let which = self.mirror_hud.cam_under(self.hud_cursor(), size);
                    let msg = match self.player.as_mut() {
                        Some(p) if shift => {
                            let n = p.vehicle.ty.def.cameras_reflexion.len();
                            p.mirror_offsets = vec![[0.0; 2]; n];
                            p.mirror_shifts = vec![[0.0; 3]; n];
                            p.mirror_fovs = vec![0.0; n];
                            p.mirrors_dirty = true;
                            "Every mirror is back as the bus has it".to_string()
                        }
                        Some(p) if which.is_some() => {
                            let i = which.unwrap_or(0);
                            if let Some(o) = p.mirror_offsets.get_mut(i) {
                                *o = [0.0; 2];
                            }
                            if let Some(s) = p.mirror_shifts.get_mut(i) {
                                *s = [0.0; 3];
                            }
                            if let Some(f) = p.mirror_fovs.get_mut(i) {
                                *f = 0.0;
                            }
                            p.mirrors_dirty = true;
                            format!("Mirror {} is back as the bus has it (Shift+R: every mirror)", i + 1)
                        }
                        _ => "R: put the cursor on a mirror panel (Shift+R: every mirror)".to_string(),
                    };
                    self.service_msg = Some((msg, 3.0));
                }
                return;
            }
            if self.mirror_hud.editing() && matches!(code, KeyCode::Insert | KeyCode::Delete | KeyCode::Backspace | KeyCode::KeyC | KeyCode::Escape) {
                if pressed && !repeat {
                    let size = self.hud_size();
                    if let Some(p) = self.player.as_ref() {
                        if let Some(msg) = self.mirror_hud.key(code, p, self.hud_cursor(), size) {
                            self.service_msg = Some((msg, 4.0));
                        }
                    }
                }
                return;
            }
        }
        // Escape closes the city map first (it would end the session)
        if pressed && code == KeyCode::Escape {
            if let Some(n) = self.navigator.as_mut().filter(|n| n.map_open()) {
                n.toggle_map();
                return;
            }
        }
        let event_key = PhysicalKey::Code(code);
        if let (Some(m), PhysicalKey::Code(code)) = (self.menu.as_mut(), event_key) {
            if pressed {
                if code == KeyCode::Escape {
                    crate::platform::exit(event_loop);
                }
                m.key(code);
                if m.start {
                    self.args.map = m
                        .maps
                        .get(m.map)
                        .map(|x| x.1.clone())
                        .unwrap_or(self.args.map.clone());
                    self.args.bus = m.vehicles.get(m.vehicle).map(|x| x.1.clone());
                    self.args.time = format!("{:02}:00", m.hour);
                    self.args.traffic = m.traffic;
                    self.args.passengers = m.passengers;
                    self.args.schedule = m.schedule;
                    self.args.day_of_year = Some(m.day);
                    if m.weather > 0 {
                        self.args.weather = m.weathers.get(m.weather).map(|w| w.1.clone());
                    }
                    if m.situation > 0 {
                        self.args.situation = m.situations.get(m.situation).map(|s| s.1.clone());
                        if let Err(e) = apply_situation(&mut self.args) {
                            log::error!("{e:#}");
                        }
                    }
                    self.menu = None;
                    self.hud = None;
                    self.load_world_now(event_loop);
                }
            }
            return;
        }
        if let PhysicalKey::Code(code) = event_key {
            let pressed = pressed;
            // LAN chat: its keys (`chat_open`, '/' or '`', and `chat_toggle`, V, in keyboard.cfg's
            // [game]: the player can move them, #130) open the line and show or hide the
            // chat, and while the line is open the keys are its own
            if let Some(l) = self.lan.as_mut() {
                let held = |a: KeyCode, b: KeyCode| self.keys.contains(&a) || self.keys.contains(&b);
                let chord = omsi_content::input::chord(
                    held(KeyCode::ShiftLeft, KeyCode::ShiftRight),
                    held(KeyCode::ControlLeft, KeyCode::ControlRight),
                    held(KeyCode::AltLeft, KeyCode::AltRight),
                );
                let bound = if held(KeyCode::SuperLeft, KeyCode::SuperRight) {
                    None
                } else {
                    keys::dik_code(code).and_then(|scan| self.game_keys.iter()
                        .find(|b| b.scan_code == scan && b.matches(chord) && b.action.to_ascii_lowercase().starts_with("chat_"))
                        .map(|b| b.action.clone()))
                };
                if lan::chat_key(l, &mut self.remotes, code, pressed, repeat, bound.as_deref()) {
                    return;
                }
            }
            if pressed && !repeat {
                self.keys.insert(code);
            } else if !pressed {
                self.keys.remove(&code);
            }
            // Alt+Enter: full screen on and off
            if pressed && !repeat && matches!(code, KeyCode::Enter | KeyCode::NumpadEnter) && (self.keys.contains(&KeyCode::AltLeft) || self.keys.contains(&KeyCode::AltRight)) {
                if self.spanned {
                    log::info!("triple screen: the window spans three monitors, Alt+Enter is left alone");
                } else if let Some(win) = self.window.as_ref() {
                    win.set_fullscreen(if win.fullscreen().is_some() { None } else { Some(winit::window::Fullscreen::Borderless(None)) });
                }
                return;
            }
            #[cfg(windows)]
            if pressed && !repeat && (self.vr.is_some() || self.settings.vr_requested()) {
                let modifier = omsi_content::input::chord(
                    self.keys.contains(&KeyCode::ShiftLeft) || self.keys.contains(&KeyCode::ShiftRight),
                    self.keys.contains(&KeyCode::ControlLeft) || self.keys.contains(&KeyCode::ControlRight),
                    self.keys.contains(&KeyCode::AltLeft) || self.keys.contains(&KeyCode::AltRight),
                );
                let action = keys::dik_code(code).and_then(|scan| self.game_keys.iter()
                    .find(|b| b.scan_code == scan && b.matches(modifier) && b.action.starts_with("vr_"))
                    .map(|b| b.action.clone()));
                if let Some(action) = action {
                    if self.game_action(&action) { return; }
                }
            }
            // Shift+number works a door the way a key bound to its trigger in
            // `Inputs/keyboard.cfg` does in OMSI: `<trigger>` when it goes down and
            // `<trigger>_off` when it comes up. The door buttons of automatic-door buses
            // are push buttons held between the two (the MAN Lion's City A21's
            // `cockpit_tuertaster1`); never let go, the button stayed pressed and, once
            // the door release was on, the front door opened and closed for ever.
            if !pressed {
                let released: Vec<Vec<String>> = if digit_of(code).is_some() {
                    self.door_key_triggers.remove(&code).into_iter().collect()
                } else if matches!(code, KeyCode::ShiftLeft | KeyCode::ShiftRight) {
                    self.door_key_triggers.drain().map(|(_, g)| g).collect()
                } else {
                    Vec::new()
                };
                if let Some(p) = self.player.as_mut() {
                    for fired in &released {
                        p.door_key_off(fired);
                    }
                }
            }
            // placing a vehicle with the mouse: its keys first (Escape takes it away)
            if self.game_menu.is_none() && self.placing_key(code, pressed) {
                return;
            }
            // the game menu: Escape opens it (and pauses, except in a LAN session, which
            // goes on for the others), and while it is open the keys are its own
            if self.game_menu.is_some() {
                if pressed && !repeat {
                    self.menu_key(event_loop, code);
                }
                return;
            }
            // the object editor takes its keys first (Escape leaves it)
            if pressed && self.editor.is_some() && self.editor_key(code) {
                return;
            }
            if pressed && !repeat && code == KeyCode::Escape {
                self.open_game_menu();
                return;
            }
            // a tutorial's pages: Enter / Page Down on, Page Up back, Ctrl+T hides them
            if let (true, Some(t)) = (pressed, self.tutorial.as_mut()) {
                let ctrl = self.keys.contains(&KeyCode::ControlLeft) || self.keys.contains(&KeyCode::ControlRight);
                match code {
                    KeyCode::Enter | KeyCode::NumpadEnter | KeyCode::PageDown if !t.hidden && self.lan.is_none() => {
                        t.next();
                        return;
                    }
                    KeyCode::PageUp if !t.hidden => {
                        t.back();
                        return;
                    }
                    KeyCode::KeyT if ctrl => {
                        t.hidden = !t.hidden;
                        return;
                    }
                    _ => {}
                }
            }
            let ctrl = self.keys.contains(&KeyCode::ControlLeft) || self.keys.contains(&KeyCode::ControlRight);
            let alt = self.keys.contains(&KeyCode::AltLeft) || self.keys.contains(&KeyCode::AltRight);
            let shift_now = self.keys.contains(&KeyCode::ShiftLeft) || self.keys.contains(&KeyCode::ShiftRight);
            // getting up (Ctrl+Shift+G) and, on foot, the walker's keys
            if self.foot_key(code, pressed, repeat, ctrl, shift_now) {
                return;
            }
            // OMSI's global actions as `Inputs/keyboard.cfg` binds them ([game]); a key our
            // driving layout uses keeps that meaning (with the OMSI layout, every binding
            // counts)
            if pressed && !repeat {
                let m = omsi_content::input::chord(shift_now, ctrl, alt);
                let own = keys::dik_code(code).is_some_and(|s| self.own_keys.contains(&s));
                let ours = self.args.drive_keys != "omsi"
                    && m == 0
                    && !own
                    && (fallback_action(code, &self.args.drive_keys).is_some()
                    || matches!(code, KeyCode::KeyZ | KeyCode::KeyX | KeyCode::KeyC | KeyCode::KeyI | KeyCode::KeyL));
                // plain Left/Right are OMSI's view_interiorcam_minus/plus, except when a wheel
                // steers: then the arrows glance (held, the head turns) and only Ctrl+Left/Right
                // switch the interior camera, below. (Where the arrows drive, `ours` skips this.)
                let plain_arrow = matches!(code, KeyCode::ArrowLeft | KeyCode::ArrowRight) && !ctrl
                    && self.controllers.as_ref().is_some_and(|c| c.wheel_steering());
                // (the keys that fly the camera are the camera's, unmodified: S, OMSI's
                // view_toggle_viewpoint, threw the free camera back to the driver's view,
                // and with no bus of one's own every view flies - #868; a chord such as
                // Ctrl+S, OMSI's quicksave, stays a [game] key)
                let flying = m == 0
                    && flies_free_camera(code)
                    && (self.view == "free" || (self.player.is_none() && self.on_foot.is_none()));
                // (Ctrl+Alt+arrows turn the mirror looked at: not Ctrl+arrow's gear or camera)
                let mirror_aim = ctrl && alt && matches!(code, KeyCode::ArrowLeft | KeyCode::ArrowRight | KeyCode::ArrowUp | KeyCode::ArrowDown);
                if let Some(scan) = keys::dik_code(code).filter(|_| !ours && !flying && !mirror_aim) {
                    let action = self.game_keys.iter().find(|b| b.scan_code == scan && b.matches(m)
                        && !b.action.starts_with("vr_")
                        && !(plain_arrow && b.action.starts_with("view_interiorcam_"))).map(|b| b.action.clone());
                    // (a key bound in [game] and in [vehicles] does both, as in Omsi.exe: the
                    // parking brake put on Space, the stock view_reset_all_directions key,
                    // reset the view and never reached the bus - #745)
                    let vehicle_too = self.player.as_ref().is_some_and(|p| p.bindings.iter().any(|b| b.scan_code == scan && b.matches(m)));
                    log::debug!("key {code:?} (DIK {scan}, chord {m}): [game] {action:?}, a key of the bus too: {vehicle_too}; [game] keys on it: {:?}", self.game_keys.iter().filter(|b| b.scan_code == scan).collect::<Vec<_>>());
                    // OMSI's `exit` (Ctrl+Q, or what the player put it on): the game ends as
                    // the menu's Quit ends it. It was no action here at all, so the key did
                    // nothing (#817)
                    if action.as_deref() == Some("exit") {
                        self.finish_vr_nav_edit();
                        self.game_menu = None;
                        self.finish_session();
                        crate::platform::exit(event_loop);
                        return;
                    }
                    if let Some(a) = action {
                        if self.game_action(&a) && !vehicle_too {
                            return;
                        }
                    }
                }
            }
            if pressed && !repeat {
                match code {
                    // OMSI's `toggel_mouse_ctrl` (O): steering and pedals with the mouse
                    KeyCode::KeyO if !ctrl && !alt && !shift_now && self.key_left_free(code, "toggel_mouse_ctrl") => {
                        self.game_action("toggel_mouse_ctrl");
                        return;
                    }
                    // (a manual gearbox's Ctrl+Up / Ctrl+Down are the [game] keys `gear_up` and
                    // `gear_down` now, which can be moved: see `with_game_defaults`, #907)
                    // (Ctrl+Alt+arrows turn the mirror looked at, see the frame)
                    KeyCode::ArrowLeft | KeyCode::ArrowRight | KeyCode::ArrowUp | KeyCode::ArrowDown if ctrl && alt => return,
                    // the interior cameras: Ctrl+Left/Right as well (the arrows drive) - unless
                    // the player gave that combination to something else (#907)
                    KeyCode::ArrowLeft | KeyCode::ArrowRight if ctrl && !self.chord_bound(code, shift_now, ctrl, alt) => {
                        self.game_action(if code == KeyCode::ArrowLeft { "view_interiorcam_minus" } else { "view_interiorcam_plus" });
                        return;
                    }
                    // OMSI's `screenshot` (Ctrl+Shift+P: 25 / 6), and F12 as most games have it
                    KeyCode::KeyP if ctrl && shift_now => {
                        self.take_screenshot();
                        return;
                    }
                    // (F12 alone only where the bus has no key of its own on it: in OMSI's
                    // keyboard.cfg it is the pram/wheelchair button, which it took away)
                    KeyCode::F12 if !self.player.as_ref().is_some_and(|p| p.bindings.iter().any(|b| b.scan_code == 88 && b.chord() == 0 && p.vehicle.ty.program.trigger(&b.action).is_some())) => {
                        self.take_screenshot();
                        return;
                    }

                    // the object editor (`crate::editor`)
                    KeyCode::KeyE if ctrl && shift_now => {
                        self.toggle_editor();
                        return;
                    }
                    // OMSI's `sim_pause`
                    KeyCode::KeyP if !ctrl && !alt && !shift_now => {
                        self.toggle_pause();
                        return;
                    }
                    // OMSI's `quicksave` (Alt+S)
                    KeyCode::KeyS if alt && !ctrl => {
                        self.quick_save();
                        return;
                    }
                    // OMSI's `view_toggle_informationdisplay` (Ctrl+Y)
                    // OMSI's `view_toggle_informationdisplay` (Shift+Y: 21 / 2)
                    KeyCode::KeyY if shift_now && !ctrl => {
                        self.info_bar = !self.info_bar;
                        return;
                    }
                    // OMSI's `view_set_schedule` (Insert: 210 / 1, the key's state every frame)
                    KeyCode::Insert if !shift_now && !ctrl => {
                        self.timetable = !self.timetable;
                        return;
                    }
                    _ => {}
                }
            }
            // the extra keys of the ready-made layouts (below): not with Custom controls, not on
            // a key the player bound, not with a modifier held
            let extras = self.args.drive_keys != "omsi"
                && !keys::dik_code(code).is_some_and(|s| self.own_keys.contains(&s))
                && !self.keys.iter().any(|k| matches!(k, KeyCode::ControlLeft | KeyCode::ControlRight | KeyCode::AltLeft | KeyCode::AltRight | KeyCode::ShiftLeft | KeyCode::ShiftRight));
            if pressed && !repeat {
                // Z / X / C: indicator left / hazard / right, where the hand rests
                // (OMSI's own layout wants Shift and the numpad for them). Each is a
                // toggle: pressing the same key again turns it back off, tracked in
                // `blinker_key_state` since the scripts expose separate "set"/"off"
                // triggers for left/right rather than a toggle (hazard already has a
                // dedicated toggle trigger, `blinker_warn_toggle`).
                if self.view != "free"
                    && extras
                    && matches!(code, KeyCode::KeyZ | KeyCode::KeyX | KeyCode::KeyC)
                {
                    let want: u8 = match code {
                        KeyCode::KeyZ => 1,
                        KeyCode::KeyC => 2,
                        _ => 3,
                    };
                    self.blinker(want);
                }
                // Shift + 1..9: open or close that physical door, front to back (see
                // `door_trigger_groups`); plain digits are left alone (some buses put
                // gears or numbered presets on them, `kw_s_1`/`automatic_1`).
                if self.view != "free" && shift_held_now(&self.keys) && !keys::dik_code(code).is_some_and(|s| self.own_shift.contains(&s)) {
                    if let Some(n) = digit_of(code) {
                        if let Some(p) = self.player.as_mut() {
                            let fire = p.door_key(n);
                            if !fire.is_empty() {
                                self.door_key_triggers.insert(code, fire);
                            }
                        }
                    }
                }
                // I: every saloon light circuit of the bus at once (OMSI has a key for
                // each: 7, 8, 9 - see Player::toggle_saloon_lights).
                if self.view != "free"
                    && !repeat
                    && extras
                    && code == KeyCode::KeyI
                {
                    if let Some(p) = self.player.as_mut() {
                        let msg = p.toggle_saloon_lights();
                        self.service_msg = Some((msg, 3.0));
                    }
                }
                // (F1-F4 where keyboard.cfg has no view keys; a key the player gave to
                // something else, or a view key moved elsewhere, leaves them alone, #701)
                match code {
                    KeyCode::F1 if self.key_left_free(code, "view_set_driver") => self.view = "driver".into(),
                    KeyCode::F2 if self.key_left_free(code, "view_set_passenger") => self.view = "pax".into(),
                    KeyCode::F3 if self.key_left_free(code, "view_set_outside") => self.view = "outside".into(),
                    KeyCode::F4 if self.key_left_free(code, "view_set_map") => {
                        // the free camera starts where the current view is looking
                        self.view = "free".into();
                        self.ego = false;
                    }
                    KeyCode::KeyU
                    if self.keys.contains(&KeyCode::ShiftLeft)
                        || self.keys.contains(&KeyCode::ShiftRight) =>
                        {
                            // Shift+U: toggle a bus's service state by itself (start a shut
                            // bus, shut down a running one), and set its IBIS to the current
                            // duty as the driver would do while putting it into service.
                            if let Some(p) = self.player.as_mut() {
                                let msg = p.start_up();
                                self.service_msg = Some((msg, 6.0));
                                if let Some(d) = self.duty.as_ref() {
                                    let (trip, stop) = d.trip_for_ibis();
                                    p.set_duty_destination(trip, stop);
                                }
                            }
                        }

                    KeyCode::KeyR
                    if self.keys.contains(&KeyCode::ShiftLeft)
                        || self.keys.contains(&KeyCode::ShiftRight) =>
                        {
                            // Shift+R: the next internet radio station (see radio.rs)
                            let msg = self.radio.next_station();
                            self.service_msg = Some((msg, 4.0));
                        }
                    KeyCode::KeyM
                    if self.keys.contains(&KeyCode::ShiftLeft)
                        || self.keys.contains(&KeyCode::ShiftRight) =>
                        {
                            // Shift+M: the city map (M alone is the starter)
                            if let Some(n) = self.navigator.as_mut() {
                                n.toggle_map();
                            }
                        }
                    KeyCode::KeyN
                    if self.keys.contains(&KeyCode::ShiftLeft)
                        || self.keys.contains(&KeyCode::ShiftRight) =>
                        {
                            // Shift+N: navigator → navigator with the schedule → off (N alone is
                            // the gearbox's neutral)
                            if self.cycle_navigator() {
                                return;
                            }
                        }
                    KeyCode::F11 => {
                        // (Ctrl+F11; F11 alone is OMSI's pedestrian view)
                        // where am I: so a place that looks wrong can be named
                        if let Some(cam) = self.camera.as_ref() {
                            let ts = omsi_map::tile_size();
                            let (tx, ty) = (
                                (cam.position.x / ts).floor() as i32,
                                (cam.position.y / ts).floor() as i32,
                            );
                            let line = format!(
                                "Position {:.0}, {:.0}, {:.1}   tile {tx}_{ty}   heading {:.0} deg",
                                cam.position.x, cam.position.y, cam.position.z, cam.yaw
                            );
                            log::info!(
                                "{line}  (--cam {:.0},{:.0},{:.0},{:.0},{:.0})",
                                cam.position.x,
                                cam.position.y,
                                cam.position.z,
                                cam.yaw,
                                cam.pitch
                            );
                            self.service_msg = Some((line, 12.0));
                        }
                    }
                    KeyCode::F9 => {
                        // write the run into the driver's personnel file
                        let line = self.career.summary();
                        if self.career.path.is_some() {
                            if let Err(e) = self.career.save() {
                                log::warn!("writing the personnel file: {e}");
                            }
                        } else {
                            log::info!("this run: {line}");
                        }
                        self.service_msg = Some((line, 8.0));
                    }
                    // F5-F8 are the destination sign and roller blind keys of OMSI's
                    // keyboard.cfg (bus_linie_minus/plus, bus_ziel_minus/plus,
                    // bus_rollband_setL1..T): they go to the bus below and nowhere else. The
                    // depot services are in the game menu (Esc), as in OMSI's menu.
                    _ => {}
                }
            }
            // the arrow keys drive when a bus is being driven (the free camera keeps them)
            // (a key the player bound to something else is theirs, not the preset's)
            let own = keys::dik_code(code).is_some_and(|s| self.own_keys.contains(&s));
            let wheel = self.controllers.as_ref().is_some_and(|c| c.wheel_steering());
            let wasd = if own {
                "omsi"
            } else if wheel {
                // (with a wheel steering, the arrow keys are OMSI's: they look around)
                match self.args.drive_keys.as_str() {
                    "arrows" | "omsi" => "omsi",
                    _ => "wasd",
                }
            } else {
                self.args.drive_keys.as_str()
            };
            let shift_held =
                self.keys.contains(&KeyCode::ShiftLeft) || self.keys.contains(&KeyCode::ShiftRight);
            if let Some(p) = self.player.as_mut() {
                let ctrl_alt_held = (self.keys.contains(&KeyCode::ControlLeft) || self.keys.contains(&KeyCode::ControlRight)) && (self.keys.contains(&KeyCode::AltLeft) || self.keys.contains(&KeyCode::AltRight));
                if self.view != "free" && !repeat && !shift_held && !(ctrl_alt_held && pressed) {
                    if let Some(a) = fallback_action(code, wasd) {
                        p.axes.set(a, pressed);
                    }
                } else if !pressed {
                    // a driving key let go always lets go: released while Shift was held (or
                    // in the free view) it stayed "pressed", and the wheel went on turning to
                    // full lock until that key was pressed again (#1040, #1050, #1053)
                    if let Some(a) = fallback_action(code, wasd) {
                        p.axes.set(a, false);
                    }
                }
            }
            // A driving key held with shift is the vehicle key it covers: Shift+W is
            // OMSI's wiper key, Shift+D selects the automatic's D, Shift+S the
            // viewpoint - otherwise a bus driven with WASD could never be put in gear.
            let shift =
                self.keys.contains(&KeyCode::ShiftLeft) || self.keys.contains(&KeyCode::ShiftRight);
            let covers_vehicle_key = fallback_action(code, wasd).is_some() && self.view != "free";
            let driving_key = covers_vehicle_key && !shift;
            // (the keys that fly the free camera are the camera's: W switched the wipers on
            // while flying)
            let fly_key = self.view == "free" && flies_free_camera(code);
            if let (Some(p), Some(scan)) = (
                self.player.as_mut(),
                keys::dik_code(code).filter(|_| !driving_key && !fly_key),
            ) {
                if !repeat {
                    let m = if covers_vehicle_key {
                        0
                    } else {
                        omsi_content::input::chord(
                            shift,
                            self.keys.contains(&KeyCode::ControlLeft) || self.keys.contains(&KeyCode::ControlRight),
                            self.keys.contains(&KeyCode::AltLeft) || self.keys.contains(&KeyCode::AltRight),
                        )
                    };
                    p.key(scan, m, pressed);
                }
            }
        }
    }

    /// LAN play, once a frame: send our bus, take in the others', and keep a drawn
    /// vehicle for each of them.
    pub(crate) fn tick_lan(&mut self, dt: f32) {
        let walker = self.walker_pose();
        let Some(lan) = self.lan.as_mut() else {
            // (the session is over: the plugin is told so)
            self.voice = None;
            return;
        };
        let duty = self
            .duty
            .as_ref()
            .map(|d| &d.trips[d.trip_index])
            .map(|t| (t.line.as_str(), t.terminus.as_str()));
        let frame = lan::Frame {
            audio: self.audio.as_ref(),
            listener: self.camera.as_ref().map(|c| c.position),
            muffled: self.in_cab || self.inside_remote.is_some(),
            riders: self.humans.as_ref().map(|h| h.riding()).unwrap_or(0),
            clock: Some(&self.clock),
            tour: self.duty.as_ref().map(|d| format!("{}/{}", d.line, d.tour)),
            walker,
            inside_of: self.inside_remote,
        };
        let updates = lan::tick(
            lan,
            &mut self.remotes,
            dt,
            &self.args,
            self.player.as_mut(),
            self.world.as_deref(),
            self.renderer.as_ref(),
            self.scene.as_mut(),
            self.traffic.as_mut(),
            self.humans.as_mut(),
            duty,
            &frame,
        );
        for u in updates {
            self.apply_world_update(u);
        }
        let cmds = self.lan.as_mut().map(|l| l.take_commands()).unwrap_or_default();
        for (from, text) in cmds {
            self.lan_command(from, &text);
        }
        self.tick_voice(dt);
    }

    /// A command another player's game sent ours (`LanSession::command`).
    pub(crate) fn lan_command(&mut self, from: u32, text: &str) {
        let Some(lan) = self.lan.as_ref() else { return };
        let my_id = lan.my_id;
        if let Some(ev) = text.strip_prefix("trigger ") {
            // a switch worked by a passenger of ours: only by one who is in our bus
            let aboard = lan.peers().find(|p| p.pose.id == from).and_then(|p| p.pose.walker).and_then(|w| w.aboard).map(|a| a.owner == my_id).unwrap_or(false);
            if !aboard {
                log::info!("LAN: player {from} asked for switch {ev} of our bus from outside it: ignored");
                return;
            }
            if let Some(p) = self.player.as_mut() {
                log::info!("LAN: player {from} works {ev} in our bus");
                p.vehicle.trigger(ev.trim());
            }
            return;
        }
        // the voice server of the session (`voice`): asked of the host, told by it
        if text == "voice?" {
            if lan.role == omsi_net::Role::Host {
                let answer = crate::voice::VoiceServer::command(crate::voice::hosted().as_ref());
                if let Some(l) = self.lan.as_mut() {
                    l.command(from, &answer);
                }
            }
            return;
        }
        if text.starts_with("voice ") {
            if from == 1 {
                if let (Some(v), Some(server)) = (self.voice.as_mut(), crate::voice::VoiceServer::parse_command(text)) {
                    v.set_server(server);
                }
            }
            return;
        }
        crate::admin::command(self, from, text);
    }

    /// The voice chat (`voice`), once a frame of a session: started with it when the
    /// settings allow, the host's voice server asked for, the plugin told who is where.
    fn tick_voice(&mut self, dt: f32) {
        let Some(lan) = self.lan.as_mut() else {
            self.voice = None;
            return;
        };
        // (a dedicated server has nobody to talk at its place; a joining game that lost
        // its host is in no session to talk in)
        if !self.settings.voice_chat || self.args.server.is_some() || !lan.connected {
            self.voice = None;
            return;
        }
        let v = self.voice.get_or_insert_with(|| crate::voice::Voice::new(crate::voice::DEFAULT_PORT));
        match lan.role {
            omsi_net::Role::Host => {
                // (once: hosted() reads voice.cfg)
                if !v.known() {
                    v.set_server(crate::voice::hosted());
                }
            }
            omsi_net::Role::Client => {
                if lan.connected && v.should_ask(dt) {
                    lan.command(1, "voice?");
                }
            }
        }
        let lan = self.lan.as_ref().unwrap();
        let my_bus = self.player.as_ref().map(|p| p.vehicle.position);
        let others = crate::voice::speakers(lan, &self.remotes, my_bus);
        let inside = if self.in_cab { Some(lan.my_id) } else { self.inside_remote };
        let listener = self.camera.as_ref().map(|c| crate::voice::Listener { at: c.position, yaw: c.yaw, inside });
        let me = (lan.my_name.clone(), lan.my_id);
        if let Some(v) = self.voice.as_mut() {
            v.tick(dt, (&me.0, me.1), listener, &others);
        }
    }

    /// The host's world as LAN play asks for it: its clock (set or caught up with) and its
    /// weather, for everything that keeps a clock of its own.
    pub(crate) fn apply_world_update(&mut self, u: lan::WorldUpdate) {
        match u {
            lan::WorldUpdate::Clock {
                year,
                day_of_year,
                time,
            } => {
                self.clock.year = year;
                self.clock.day_of_year = day_of_year;
                self.clock.time = time;
                if let Some(t) = self.traffic.as_mut() {
                    t.day_time = time;
                }
            }
            lan::WorldUpdate::Slew(s) => {
                self.clock.time = (self.clock.time + s).clamp(0.0, 86399.999);
                if let Some(t) = self.traffic.as_mut() {
                    t.day_time += s;
                }
            }
            lan::WorldUpdate::Weather(w) => {
                log::info!(
                    "LAN: the host's weather: {}",
                    w.as_deref().unwrap_or("the map's default")
                );
                // (coming over to it as the host does, not at a stroke - the streets stay
                // as wet as they are and dry or wet with it)
                self.change_weather(w, false, 240.0);
            }
            lan::WorldUpdate::Tours(tours) => {
                if let Some(s) = self.schedule.as_mut() {
                    s.set_lan_tours(tours);
                }
            }
        }
        if let Some(p) = self.player.as_mut() {
            p.vehicle.host.clock = self.clock.clone();
        }
    }

    /// Shift+N: the navigator, the navigator with the schedule, off. True in VR (where the
    /// key is used up).
    pub(crate) fn cycle_navigator(&mut self) -> bool {
        if self.vr_active() {
            if !self.vr_nav_profile().enabled {
                self.vr_nav_adjust("enabled", 1.0);
            } else if self.navigator.as_ref().is_some_and(|n| n.schedule) {
                if let Some(n) = self.navigator.as_mut() { n.schedule = false; }
                self.vr_nav_adjust("enabled", 1.0);
            } else if let Some(n) = self.navigator.as_mut() {
                n.schedule = true;
            }
            return true;
        }
        if let Some(n) = self.navigator.as_mut() {
            match (n.enabled, n.schedule) {
                (true, false) => n.schedule = true,
                (true, true) => {
                    n.enabled = false;
                    n.schedule = false;
                }
                _ => n.enabled = true,
            }
        }
        false
    }

    /// Whether a key the game gives `action` by itself (F1 the driver's view, O the mouse
    /// steering) is still free for it: neither bound by the player to something of their
    /// own nor `action` bound to another key in keyboard.cfg.
    pub(crate) fn key_left_free(&self, code: KeyCode, action: &str) -> bool {
        key_left_free(keys::dik_code(code), action, &self.own_keys, &self.game_keys)
    }

    /// Turn the view by (dx, dy) degrees, as dragging with the right button does: the free
    /// camera turns, inside the bus the head turns, outside the camera swings around it.
    /// Keeps `look` with the view it belongs to: on a change of view the direction of the
    /// view left is put away and the one of the view entered comes back (straight ahead
    /// the first time).
    pub(crate) fn sync_view_look(&mut self) {
        let key = self.look_key();
        swap_view_look(&mut self.look, &mut self.view_looks, &mut self.look_view, &key);
    }

    /// Which camera the look belongs to: the view, and for the driver's and the passengers'
    /// view the camera chosen in it. Each of Omsi.exe's cameras keeps where it was turned
    /// (a `TCamera` has its own yaw and pitch besides the file's, 0x7edde4 resets them): the
    /// look went back to straight ahead whenever the viewpoint changed.
    pub(crate) fn look_key(&self) -> String {
        look_key_of(&self.view, self.player.as_ref().map(|p| p.cam_choice))
    }

    /// Zoom the view inside the bus by `notches` of the mouse wheel (in: positive).
    pub(crate) fn zoom_by(&mut self, notches: f32) {
        let z = self.view_zoom.entry(self.view.clone()).or_insert(1.0);
        *z = (*z * (1.0 - 0.08 * notches.clamp(-5.0, 5.0))).clamp(0.2, 1.6);
    }

    pub(crate) fn look_by(&mut self, dx: f32, dy: f32) {
        self.sync_view_look();
        if self.view == "foot" {
            self.foot_look(dx, dy);
            return;
        }
        if self.view == "free" || self.player.is_none() {
            if let Some(cam) = self.camera.as_mut() {
                cam.yaw = (cam.yaw + dx).rem_euclid(360.0);
                cam.pitch = (cam.pitch - dy).clamp(-89.0, 89.0);
            }
        } else if self.view == "outside" {
            // F3 chase orbit: full turn in yaw; pitch stops between near
            // top-down and just below eye level so the camera never swings
            // under the bus (see `chase_orbit_step` for the mouse gain).
            self.look.0 = (self.look.0 + dx).rem_euclid(360.0);
            self.look.1 = (self.look.1 - dy).clamp(-60.0, 25.0);
        } else {
            self.look.0 = cab_look_yaw(&self.view, self.look.0 + dx);
            self.look.1 = (self.look.1 - dy).clamp(-85.0, 85.0);
        }
    }

    /// The cursor moved to (x, y) in physical pixels - from the window or an `OMSI_INPUT` script.
    /// The cursor moved, and the switch under it is named at once (touch input and
    /// `OMSI_INPUT` scripts read `hover` right after).
    pub(crate) fn on_cursor(&mut self, x: f32, y: f32) {
        if self.move_cursor(x, y) {
            self.update_hover();
        }
    }

    /// The window's `CursorMoved`: only the cursor is taken; the switch under it is named
    /// once a frame (`RedrawRequested`). A gaming mouse sends 500-8000 moves a second, and a
    /// ray through every cockpit mesh for each of them kept the event queue from ever
    /// draining - no frame was drawn while the mouse moved.
    /// The indicator lever: 1 left, 2 right, 3 the hazard lights - each a toggle, as the
    /// Z / C / X keys and the phone's buttons work it.
    pub(crate) fn blinker(&mut self, want: u8) {
        if let Some(player) = self.player.as_mut() {
            player.toggle_indicator(want);
        }
    }

    /// The right mouse button (or both) held in a view of the bus: start OMSI's mouse zoom
    /// (false when there is nothing to zoom - a menu, the city map, on foot).
    pub(crate) fn start_both_drag(&mut self) -> bool {
        if self.game_menu.is_some() || self.player.is_none() || self.navigator.as_ref().is_some_and(|n| n.map_open()) {
            return false;
        }
        let value = match self.view.as_str() {
            "outside" => self.orbit,
            "driver" | "pax" => *self.view_zoom.get(&self.view).unwrap_or(&1.0),
            _ => return false,
        };
        self.both_drag = Some((self.cursor.1, value));
        self.mouse_look = false;
        self.update_hover();
        true
    }

    /// Whether the mouse steering (when on) steers in the view shown: every view of the
    /// player's bus, the map camera (F4) included, but not walking.
    pub(crate) fn mouse_steers_in_view(&self) -> bool {
        self.player.is_some() && (matches!(self.view.as_str(), "driver" | "outside" | "pax") || (self.view == "free" && !self.ego))
    }

    /// Looking round with the mouse goes by the cursor's way in the window (a view of the
    /// bus); on foot and with the free camera it keeps the raw mouse movement.
    pub(crate) fn cursor_looks(&self) -> bool {
        self.mouse_look && self.player.is_some() && !matches!(self.view.as_str(), "foot" | "free")
    }

    /// The right button alone zooms, as in Omsi.exe (TForm_main.Panel1MouseMove 0x82c5f8:
    /// ssRight without `[altView]`, or Shift+right with it); otherwise it turns the view.
    pub(crate) fn right_zooms(&self) -> bool {
        !self.settings.alt_view || self.keys.contains(&KeyCode::ShiftLeft) || self.keys.contains(&KeyCode::ShiftRight)
    }

    /// The right mouse button on the desktop: held, it zooms (the outside camera's distance,
    /// the view in the bus), or with OMSI's `[altView]` turns the view; the middle button
    /// turns it in any case. Where there is nothing to zoom it turns the view.
    pub(crate) fn on_right(&mut self, pressed: bool) {
        self.buttons_held.1 = pressed;
        // the left button already down on nothing it works: both held zoom
        if pressed && self.buttons_held.0 && !self.dragging && self.start_both_drag() {
            return;
        }
        // (a switch held with the left button keeps the mouse: looking round
        // took the cursor's movement away from it, and the drag stopped)
        if pressed && self.dragging {
            return;
        }
        if !pressed {
            self.both_drag = None;
        }
        // a right click lets go of the mouse steering as in OMSI (#162) when the player
        // wants it so; otherwise the right button looks round and the wheel and pedals stay
        // where the mouse left them (it went off with every look round, and with every
        // look round in the pause)
        if pressed && self.mouse_drive && self.game_menu.is_none() && self.settings.mouse_right_off && !self.paused {
            self.set_mouse_drive(false);
            self.service_msg = Some(("Mouse steering off".into(), 3.0));
        }
        if pressed && self.right_zooms() && self.start_both_drag() {
            return;
        }
        if self.mouse_drive && self.game_menu.is_none() {
            if pressed {
                self.steer_cursor = Some(self.cursor);
            } else if let Some((x, y)) = self.steer_cursor.take() {
                self.cursor = (x, y);
                if let Some(win) = self.window.as_ref() {
                    let _ = win.set_cursor_position(winit::dpi::PhysicalPosition::new(x as f64, y as f64));
                }
            }
        }
        self.mouse_look = pressed;
        // (the cursor shows it at once, not with the next look at what is under it)
        self.update_hover();
    }

    pub(crate) fn on_mouse_moved(&mut self, x: f32, y: f32) {
        // a mirror panel being dragged follows the cursor (nothing else of the cursor's
        // work is done meanwhile, and outside a drag none of it is touched)
        if self.mirror_hud.dragging() {
            if let Some(size) = self.surface.as_ref().map(|_| self.hud_size()) {
                let origin_x = self.cursor.0 - self.hud_cursor().0;
                if self.mirror_hud.moved((x - origin_x, y), size) {
                    self.cursor = (x, y);
                    return;
                }
            }
        }
        if self.move_cursor(x, y) {
            self.html_move();
        }
    }

    fn html_move(&mut self) {
        if let Some((id, page, ..)) = self.html_object_pressed {
            let Some((o, d, _)) = self.cursor_ray_now() else { return };
            let Some(w) = self.world.clone() else { return };
            if let Some(h) = w.html_object_hit(o, d, HTML_OBJECT_REACH).filter(|h| h.map_id == id && h.page == page) {
                w.html_object_pointer(id, page, h.u, h.v, omsi_sim::htmltex::PointerKind::Move);
                self.html_object_pressed = Some((id, page, h.u, h.v));
            }
            return;
        }
        let Some((page, ..)) = self.html_pressed else { return };
        let Some((o, d, _)) = self.cursor_ray_now() else { return };
        let Some(p) = self.player.as_mut() else { return };
        if let Some((pg, u, v)) = p.html_hit(o, d).filter(|h| h.0 == page) {
            p.html_pointer(pg, u, v, omsi_sim::htmltex::PointerKind::Move);
            self.html_pressed = Some((pg, u, v));
        }
    }

    #[cfg(windows)]
    pub(crate) fn reset_vr_pointer(&mut self) {
        if let Some(vr) = self.vr.as_mut() {
            vr.recenter_pointer();
        }
        self.vr_cursor_physical = None;
        self.vr_cursor_warp_pending = None;
    }

    #[cfg(windows)]
    pub(crate) fn on_vr_cursor_moved(&mut self, x: f32, y: f32) {
        if let Some(target) = self.vr_cursor_warp_pending.take() {
            // CursorMoved from set_cursor_position is not hand movement.
            self.vr_cursor_physical = Some((x, y));
            if (x - target.0).abs() < 3.0 && (y - target.1).abs() < 3.0 {
                return;
            }
            // A real move arrived first; use the next event as the new baseline.
            return;
        }
        if let Some(previous) = self.vr_cursor_physical {
            self.cursor.0 += x - previous.0;
            self.cursor.1 += y - previous.1;
        }
        self.vr_cursor_physical = Some((x, y));
        self.html_move();
        let Some((width, height)) = self.surface.as_ref().map(|s|
            (s.config.width as f32, s.config.height as f32)) else { return };
        if self.window_focused && !self.mouse_look
            && (x < 12.0 || x > width - 12.0 || y < 12.0 || y > height - 12.0) {
            let center = (width * 0.5, height * 0.5);
            if self.window.as_ref().is_some_and(|window| window.set_cursor_position(
                winit::dpi::PhysicalPosition::new(center.0 as f64, center.1 as f64)).is_ok()) {
                self.vr_cursor_physical = Some(center);
                self.vr_cursor_warp_pending = Some(center);
            }
        }
    }

    #[cfg(windows)]
    pub(crate) fn poll_vr_cursor_position(&mut self) {
        if self.vr_nav_edit.is_some() { return; }
        let cockpit = self.vr.is_some() && self.game_menu.is_none()
            && self.chooser.is_none() && !self.mouse_drive
            && matches!(self.view.as_str(), "driver" | "pax");
        if !cockpit {
            self.vr_cursor_physical = None;
            self.vr_cursor_warp_pending = None;
            return;
        }
        if !self.window_focused || self.mouse_look { return; }
        let Some(window) = self.window.as_ref() else { return };
        let Ok(client_origin) = window.inner_position() else { return };
        let mut point = windows::Win32::Foundation::POINT::default();
        if unsafe { windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut point) }.is_ok() {
            self.on_vr_cursor_moved((point.x - client_origin.x) as f32,
                                    (point.y - client_origin.y) as f32);
        }
    }

    /// Mouse steering beyond the window's edge: with the cursor pinned at the left or right
    /// edge, the mouse moving on outwards turns the wheel further (the whole width per full
    /// lock, as standing); moving back gives that back first, the cursor held at the edge
    /// until it is used up, so the wheel never jumps.
    pub(crate) fn mouse_past_edge(&mut self, dx: f32) {
        let Some(w) = self.surface.as_ref().map(|s| s.config.width as f32) else { return };
        let per_px = 2.0 / w.max(1.0);
        let (at_left, at_right) = (self.cursor.0 <= 2.0, self.cursor.0 >= w - 3.0);
        let before = self.mouse_edge;
        if (at_right && dx > 0.0) || (at_left && dx < 0.0) {
            self.mouse_edge = (self.mouse_edge + dx * per_px).clamp(-2.0, 2.0);
        } else if (self.mouse_edge > 0.0 && dx < 0.0) || (self.mouse_edge < 0.0 && dx > 0.0) {
            let m = self.mouse_edge + dx * per_px;
            self.mouse_edge = if m.signum() != before.signum() { 0.0 } else { m };
            // (the cursor stays where it was: the move went into the wheel)
            if let Some(win) = self.window.as_ref() {
                let x = if before > 0.0 { w - 2.0 } else { 1.0 };
                let _ = win.set_cursor_position(winit::dpi::PhysicalPosition::new(x as f64, self.cursor.1 as f64));
                self.cursor.0 = x;
            }
        }
    }

    /// Take the cursor's new place; false when the move was someone else's (the object
    /// editor's drag, the city map) and no switch is to be named.
    fn move_cursor(&mut self, x: f32, y: f32) -> bool {
        let last = self.cursor;
        self.cursor = (x, y);
        // the navigator held by the mouse follows it
        if let Some(n) = self.navigator.as_mut() {
            if n.panel_move(x, y) {
                return true;
            }
        }
        // (the mouse has taken over from the keyboard: only what is under it is lit)
        if self.game_menu.is_some() && (x, y) != last {
            self.menu_kbd = false;
        }
        // a slider held with the mouse button follows the cursor (while the menu is open)
        if self.menu_drag.is_some() && self.game_menu.is_none() {
            self.menu_drag = None;
        }
        if let Some(k) = self.menu_drag {
            let c = self.ui.as_ref().and_then(|u| k.checked_sub(u.menu_start).and_then(|i| u.menu_ctl.get(i).copied().flatten()));
            match c {
                Some(c) => {
                    let fx = ((x - c[0]) / (c[2] - c[0]).max(1.0)).clamp(0.0, 1.0);
                    self.list_click(k, fx);
                }
                None => self.menu_drag = None,
            }
            return false;
        }
        if let Some((y0, v0)) = self.both_drag {
            // (0x82c5f8: outside, the distance at the press times 1 + the way up over 500
            // pixels; in the bus the field of view at the press plus the way up over 500
            // pixels times the camera's own, which is also its widest (+0x31c, 0x7edde4):
            // moving up widens the view as it backs the outside camera away)
            if self.view == "outside" {
                let k = (1.0 + (y0 - y) / 500.0).max(0.05);
                self.orbit = (v0 * k).clamp(ORBIT_MIN, ORBIT_MAX);
            } else {
                self.view_zoom.insert(self.view.clone(), (v0 + (y0 - y) / 500.0).clamp(0.2, 1.0_f32.max(v0)));
            }
            return false;
        }
        if self.pane_scroll_drag.is_some() {
            if self.chooser.is_none() || self.game_menu.is_none() {
                self.pane_scroll_drag = None;
            } else {
                self.drag_pane(y);
                return false;
            }
        }
        if self.dd_scroll_drag.is_some() {
            if self.dropdown.is_none() || self.game_menu.is_none() {
                self.dd_scroll_drag = None;
            } else {
                self.drag_dropdown(y);
                return false;
            }
        }
        if self.menu_scroll_drag {
            let Some(ui) = self.ui.as_ref() else {
                self.menu_scroll_drag = false;
                return true;
            };

            if let (Some(track), Some(thumb)) =
                (ui.menu_scroll_track, ui.menu_scroll_thumb)
            {
                let track_h = (track[3] - track[1]).max(1.0);
                let thumb_h = (thumb[3] - thumb[1]).max(1.0);
                let travel = (track_h - thumb_h).max(1.0);

                let max_top =
                    (self.menu_len() as f32 - ui.menu_rows as f32).max(0.0);

                if max_top > 0.0 {
                    let delta = (y - last.1) / travel * max_top;

                    self.menu_top = Some(
                        (self.menu_top.unwrap_or(ui.menu_start as f32) + delta)
                            .clamp(0.0, max_top),
                    );
                }
            }

            return false;
        }
        // an object dragged in the object editor follows
        if self.editor_drag {
            self.editor_drag_frame();
            return false;
        }
        // while the city map is open the mouse is the map's
        if let Some(n) = self.navigator.as_mut().filter(|n| n.map_open()) {
            n.map_move(x, y);
            return false;
        }
        // looking round in a view of the bus follows the cursor, as Omsi.exe turns it
        // (0x82c5f8: yaw and pitch at the press plus the cursor's way times fov / 78.75):
        // raw device deltas are no window pixels (a tablet, a remote desktop or a VM
        // reports positions there and spun the view) and did not follow the zoom
        if self.cursor_looks() {
            let scale = self.window.as_ref().map(|w| w.scale_factor() as f32).unwrap_or(1.0).max(0.1);
            let fov = self.camera.as_ref().map(|c| c.fov_deg).unwrap_or(60.0);
            let k = look_deg_per_px(fov) * self.settings.look_sens;
            self.look_by((x - last.0) / scale * k, (y - last.1) / scale * k);
        }
        // Dragging a switch reads the movement in screen pixels - take it from the
        // cursor itself rather than from the raw device delta, which is not in the
        // window's pixels (and on this Mac is not always delivered at all): that is
        // why the parking brake could not be pulled with the mouse. The movement is
        // collected here and handed to the script once a frame (`App::drag_frame`).
        if self.dragging {
            let scale = self
                .window
                .as_ref()
                .map(|w| w.scale_factor() as f32)
                .unwrap_or(1.0)
                .max(0.1);
            self.drag_delta.0 += (self.cursor.0 - last.0) / scale;
            self.drag_delta.1 += (self.cursor.1 - last.1) / scale;
        }
        true
    }

    pub(crate) fn on_left(&mut self, pressed: bool) {
        if self.vr_nav_edit.is_some() { return; }
        // the object editor: the mouse picks and drags
        if self.game_menu.is_none() && self.editor_mouse(pressed) {
            return;
        }
        // the city map: a click on the navigator opens it; while it is open the mouse is
        // the map's (a click outside closes it)
        let (x, y) = self.cursor;
        let vr_active = self.vr_active();
        if let Some(n) = self.navigator.as_mut() {
            if n.map_open() {
                let ctrl = self.keys.contains(&KeyCode::ControlLeft) || self.keys.contains(&KeyCode::ControlRight);
                if pressed && (ctrl || self.teleport_pick) {
                    // Ctrl+click (or a click after Esc → Move the bus): the bus to the street
                    // nearest that point, as OMSI's map window places vehicles
                    if let Some(at) = n.map_point(x, y) {
                        if std::mem::take(&mut self.teleport_pick) {
                            n.toggle_map();
                        }
                        self.place_bus_at(at);
                    }
                } else if pressed {
                    n.map_press(x, y);
                } else {
                    n.map_release();
                }
                return;
            }
            // (a click opens the city map, a drag moves the navigator: #940)
            if pressed && !vr_active && n.over_panel(x, y) {
                n.panel_press(x, y);
                return;
            }
            if !pressed {
                match n.panel_release() {
                    Some(false) => {
                        n.toggle_map();
                        return;
                    }
                    Some(true) => {
                        let at = n.placement();
                        self.settings.navigator_corner = at.clone();
                        crate::game_lists::remember_setting("navigator_corner", &at);
                        return;
                    }
                    None => {}
                }
            }
        }
        // the map camera (F4): Ctrl+click on the ground puts the bus on the street nearest
        // that point, as Ctrl+click on the city map does - OMSI's map view moves the vehicle
        // to a place clicked as well (#1039). A rail vehicle stays on its track.
        let ctrl = self.keys.contains(&KeyCode::ControlLeft) || self.keys.contains(&KeyCode::ControlRight);
        if pressed && ctrl && self.view == "free" && self.game_menu.is_none() && self.player.is_some() {
            if self.player.as_ref().is_some_and(|p| crate::rail_drive::is_rail(&p.vehicle.ty.def)) {
                self.service_msg = Some(("A rail vehicle cannot be moved off its track".into(), 3.0));
                return;
            }
            let hit = self
                .cursor_ray_now()
                .zip(self.world.clone())
                .and_then(|((o, d, _), w)| crate::placing::ground_hit(&w, o, d.as_dvec3(), 2000.0));
            match hit {
                Some(at) => self.place_bus_at(at.truncate()),
                None => self.service_msg = Some(("Ctrl+click on the ground to move the bus there".into(), 3.0)),
            }
            return;
        }
        // a click on the chat opens its input box (and is the chat's, not the cockpit's)
        if pressed && self.lan.is_some() && self.settings.chat {
            if self.ui.as_ref().map(|u| u.chat.hovered).unwrap_or(false) {
                self.remotes.chat.open();
                return;
            }
            // a click anywhere else leaves the line and goes on to the game
            self.remotes.chat.blur();
        }
        // in another player's bus: a passenger, whose clicks work nothing of it (they
        // went to the driver's game, which worked its switches for them)
        if self.view == "foot" && self.inside_remote.is_some() {
            return;
        }
        // a page (`[htmltexture]`) on a scenery object: pressed and released like the bus's own
        if self.html_object_click(pressed) {
            return;
        }
        // on foot: the own bus's switches, doors and flaps from inside it or standing by it
        if self.view == "foot" && !self.foot_reaches_bus() {
            return;
        }
        #[cfg(windows)]
        if self.vr.is_some() && self.mouse_drive && self.game_menu.is_none()
            && matches!(self.view.as_str(), "driver" | "pax") {
            if !pressed {
                if let Some(player) = self.player.as_mut() { player.release(); }
                self.dragging = false;
            }
            return;
        }
        let ray = self.camera.as_ref().zip(self.surface.as_ref())
            .map(|(cam, s)| self.cockpit_cursor_ray(cam, (s.config.width, s.config.height)));
        if let (Some(p), Some((o, d, spread))) = (
            self.player.as_mut(),
            ray,
        ) {
            self.drag_delta = (0.0, 0.0);
            p.occlude_controls = self.view == "outside";
            if pressed {
                if let Some((page, u, v)) = p.html_hit(o, d) {
                    p.release();
                    p.html_pointer(page, u, v, omsi_sim::htmltex::PointerKind::Down);
                    self.html_pressed = Some((page, u, v));
                    self.dragging = false;
                    return;
                }
                self.dragging = p
                    .click(o, d, spread)
                    .is_some();
            } else {
                if let Some((page, u, v)) = self.html_pressed.take() {
                    let (u, v) = p.html_hit(o, d).filter(|h| h.0 == page).map_or((u, v), |h| (h.1, h.2));
                    p.html_pointer(page, u, v, omsi_sim::htmltex::PointerKind::Up);
                    self.dragging = false;
                    return;
                }
                if self.dragging && self.buttons_held.1 {
                    p.release_keeping();
                } else {
                    p.release();
                }
                self.dragging = false;
            }
        }
    }

    /// A click on a page of a scenery object (`[htmltexture]` in its model). True when the
    /// click was the page's: the press lands on it, the release goes to it wherever the
    /// pointer is by then.
    fn html_object_click(&mut self, pressed: bool) -> bool {
        let Some(w) = self.world.clone() else { return false };
        if !pressed {
            let Some((id, page, u, v)) = self.html_object_pressed.take() else { return false };
            let (u, v) = self
                .cursor_ray_now()
                .and_then(|(o, d, _)| w.html_object_hit(o, d, HTML_OBJECT_REACH))
                .filter(|h| h.map_id == id && h.page == page)
                .map_or((u, v), |h| (h.u, h.v));
            w.html_object_pointer(id, page, u, v, omsi_sim::htmltex::PointerKind::Up);
            self.dragging = false;
            return true;
        }
        // (driving with the VR pointer: the clicks are the bus's)
        #[cfg(windows)]
        if self.vr.is_some() && self.mouse_drive && self.game_menu.is_none() && matches!(self.view.as_str(), "driver" | "pax") {
            return false;
        }
        let Some((o, d, _)) = self.cursor_ray_now() else { return false };
        let Some(h) = w.html_object_hit(o, d, HTML_OBJECT_REACH) else { return false };
        // the bus in front of the page (a switch, a window, its own page) takes the click
        if self.player.as_ref().and_then(|p| p.body_hit(o, d)).is_some_and(|t| t < h.t) {
            return false;
        }
        if let Some(p) = self.player.as_mut() {
            p.release();
        }
        w.html_object_pointer(h.map_id, h.page, h.u, h.v, omsi_sim::htmltex::PointerKind::Down);
        self.html_object_pressed = Some((h.map_id, h.page, h.u, h.v));
        self.dragging = false;
        true
    }

    /// The ray under the cursor now (see [`Self::cockpit_cursor_ray`]).
    fn cursor_ray_now(&self) -> Option<(glam::DVec3, glam::Vec3, f32)> {
        let (cam, s) = self.camera.as_ref().zip(self.surface.as_ref())?;
        Some(self.cockpit_cursor_ray(cam, (s.config.width, s.config.height)))
    }

    /// A switch held with the mouse: OMSI runs its `<event>_drag` trigger every frame the
    /// button is down, with this frame's movement in `mouse_x` / `mouse_y` - 0 while the
    /// hand keeps still. The scripts rely on that: the EN92 cash desk takes its swing speed
    /// from the last two positions, and fired only on movement it kept the speed of the last
    /// small move through a pause and swung shut when let go; the door scripts set their
    /// push once per trigger.
    pub(crate) fn drag_frame(&mut self) {
        if !self.dragging {
            return;
        }
        let (dx, dy) = std::mem::take(&mut self.drag_delta);
        if let Some(p) = self.player.as_mut() {
            p.drag(dx, dy);
        }
    }

    /// A key of the input script by name: a letter, a digit, F1..F12, or one of the named keys.
    pub(crate) fn script_key(name: &str) -> Option<KeyCode> {
        use KeyCode::*;
        pub(crate) const LETTERS: [KeyCode; 26] = [KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN, KeyO, KeyP, KeyQ, KeyR, KeyS, KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ];
        pub(crate) const DIGITS: [KeyCode; 10] = [Digit0, Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9];
        pub(crate) const FKEYS: [KeyCode; 12] = [F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12];
        let b = name.as_bytes();
        if b.len() == 1 && b[0].is_ascii_alphabetic() {
            return Some(LETTERS[(b[0].to_ascii_uppercase() - b'A') as usize]);
        }
        if b.len() == 1 && b[0].is_ascii_digit() {
            return Some(DIGITS[(b[0] - b'0') as usize]);
        }
        if let Some(n) = name.strip_prefix('F').and_then(|n| n.parse::<usize>().ok()) {
            return FKEYS.get(n.wrapping_sub(1)).copied();
        }
        Some(match name {
            "Shift" => ShiftLeft,
            "Ctrl" => ControlLeft,
            "Alt" => AltLeft,
            "." => Period,
            "," => Comma,
            "Up" => ArrowUp,
            "Down" => ArrowDown,
            "Left" => ArrowLeft,
            "Right" => ArrowRight,
            "Enter" => Enter,
            "Escape" => Escape,
            "Backspace" => Backspace,
            "Space" => Space,
            "PageUp" => PageUp,
            "PageDown" => PageDown,
            "Insert" => Insert,
            "Home" => Home,
            "End" => End,
            "Delete" => Delete,
            "[" => BracketLeft,
            "]" => BracketRight,
            _ => return None,
        })
    }

    /// `OMSI_INPUT`: scripted window input, so the very same handlers the mouse and keyboard
    /// reach can be driven from the command line and checked without a hand on the mouse:
    /// `t=3 move 1045,826; t=3.2 press; t=3.5 drag 0,-80; t=4 release; t=4.5 log bremse_feststell;
    /// t=5 key F3` - coordinates in logical pixels, `drag` relative, `key` a winit key name;
    /// also `look yaw,pitch` (turn the head to), `turn dx,dy` (turn the view by degrees, as a
    /// right-button drag does, the free and outside cameras too), `set name=value` (a script
    /// variable), `trigger name`, `log name` (with what the HUD says about the cursor),
    /// `dumptex <folder>` (the display pictures), `shot <file>` (the window's picture as a
    /// PNG) and `type <text>` (into the LAN chat line that `key V` opened; `key Enter`
    /// sends it).
    pub(crate) fn run_input_script(&mut self, event_loop: &ActiveEventLoop) {
        if self.input_script.is_empty() {
            return;
        }
        let t = self.started.elapsed().as_secs_f32();
        let scale = self
            .window
            .as_ref()
            .map(|w| w.scale_factor() as f32)
            .unwrap_or(1.0);
        while let Some((at, cmd)) = self.input_script.first().cloned() {
            if t < at {
                break;
            }
            self.input_script.remove(0);
            let mut parts = cmd.split_whitespace();
            let verb = parts.next().unwrap_or("");
            let arg = parts.next().unwrap_or("");
            let xy = || -> (f32, f32) {
                let mut it = arg.split(',').filter_map(|v| v.trim().parse::<f32>().ok());
                (it.next().unwrap_or(0.0), it.next().unwrap_or(0.0))
            };
            log::info!("input script t={t:.1}: {cmd}");
            match verb {
                "move" => {
                    let (x, y) = xy();
                    self.on_cursor(x * scale, y * scale);
                }
                // `weather`: the next weather, as the admin menu's "Next weather"
                "weather" => self.next_weather(),
                // `rawmouse dx`: the mouse moved by dx device units (past the window's edge
                // too, as mouse steering takes it)
                "rawmouse" => {
                    let (dx, _) = xy();
                    if self.mouse_drive && self.game_menu.is_none() {
                        self.mouse_past_edge(dx);
                    }
                }
                "drag" => {
                    let (dx, dy) = xy();
                    let (x, y) = self.cursor;
                    self.on_cursor(x + dx * scale, y + dy * scale);
                }
                // `look yaw,pitch`: turn the head, as a right-drag does
                // `touch down|move|up x,y[,id]`: a finger (logical pixels), as the phone's
                // screen gives it to the on-screen controls (OMSI_TOUCH=1 shows them)
                "touch" => {
                    let rest = parts.next().unwrap_or("");
                    let mut it = rest.split(',').filter_map(|v| v.trim().parse::<f32>().ok());
                    let (x, y) = (it.next().unwrap_or(0.0), it.next().unwrap_or(0.0));
                    let id = it.next().unwrap_or(0.0) as u64;
                    self.script_touch(event_loop, arg, x * scale, y * scale, id);
                }
                "look" => self.look = xy(),
                // `orbit <m>`: how far the outside camera stands off, as the mouse wheel sets it
                "orbit" => self.orbit = xy().0.clamp(ORBIT_MIN, ORBIT_MAX),
                // `set name=value`: put a script variable somewhere (a switch half way)
                "set" => {
                    if let (Some((k, v)), Some(p)) = (arg.split_once('='), self.player.as_mut()) {
                        let ok = p.vehicle.set_var(k.trim(), v.trim().parse().unwrap_or(0.0));
                        log::info!("input script: set {k} -> {ok}");
                    }
                }
                "trigger" => {
                    if let Some(p) = self.player.as_mut() {
                        let ok = p.vehicle.trigger(arg);
                        log::info!("input script: trigger {arg} -> {ok}");
                    }
                }
                // `wheel <notches>`: the mouse wheel, where the placing and the menu take it
                "wheel" => {
                    let n = xy().0;
                    if self.editor.is_some() && self.game_menu.is_none() {
                        self.editor_wheel(n);
                    } else if self.placing.is_some() && self.game_menu.is_none() {
                        self.placing_wheel(n);
                    } else if self.game_menu.is_some() {
                        self.menu_wheel(n);
                    } else {
                        self.wheel(n);
                    }
                    log::info!("input script: wheel {n}: menu line {:?}, chooser {:?}, placing heading {:?}", self.game_menu, self.chooser, self.placing.as_ref().map(|p| p.heading));
                }
                // `click`: a left click where the cursor is, through the window's own path;
                // `click down` / `click up` only press or let go (a drag in between)
                "click" => {
                    let (press, release) = (arg != "up", arg != "down");
                    if self.placing.is_some() && self.game_menu.is_none() {
                        self.placing_click();
                    } else if self.game_menu.is_some() {
                        // (on the menu as the window's button: its lines, its arrows)
                        if press {
                            self.left_button(event_loop, true);
                        }
                        if release {
                            self.left_button(event_loop, false);
                        }
                    } else {
                        if press {
                            self.on_left(true);
                        }
                        if release {
                            self.on_left(false);
                        }
                    }
                    log::info!("input script: click: placing {:?}, placed at {:?}", self.placing.as_ref().map(|p| (p.at, p.blocked)), self.placed.last().map(|q| (q.vehicle.position, q.vehicle.heading)));
                }
                // `both down|up`: both mouse buttons held (OMSI's mouse zoom) or let go
                "both" => {
                    if arg == "down" {
                        self.buttons_held = (true, true);
                        let started = self.start_both_drag();
                        log::info!("input script: both buttons: zoom drag {started}");
                    } else {
                        self.buttons_held = (false, false);
                        self.both_drag = None;
                        log::info!("input script: both buttons up: zoom {:?}, orbit {:.1}", self.view_zoom.get(&self.view), self.orbit);
                    }
                }
                // `right down|up`: the right mouse button, through the window's own path
                "right" => {
                    self.on_right(arg == "down");
                    log::info!("input script: right button {arg}: zoom drag {}, look {}, zoom {:?}, orbit {:.1}", self.both_drag.is_some(), self.mouse_look, self.view_zoom.get(&self.view), self.orbit);
                }
                "press" => self.on_left(true),
                "release" => self.on_left(false),
                // `type <text>`: characters into the open LAN chat line (after `key V`)
                "type" => {
                    let text = cmd.split_once(' ').map(|x| x.1).unwrap_or("");
                    if lan::chat_open(&self.remotes) {
                        lan::chat_type(&mut self.remotes, text);
                    } else {
                        log::warn!("input script: the chat line is not open");
                    }
                }
                // `turn dx,dy`: turn the view by degrees, as a right-button drag does
                "turn" => {
                    let (dx, dy) = xy();
                    self.look_by(dx, dy);
                }
                // `focus 0|1`, `minimize`, `restore`: the window losing and getting back the
                // keyboard and the mouse, as the window's own events do it
                "focus" if arg == "0" => self.input_lost(),
                "focus" => self.input_back(),
                "minimize" => {
                    self.window_hidden = true;
                    self.input_lost();
                }
                "restore" => {
                    self.window_hidden = false;
                    self.input_back();
                }
                "key" | "keydown" | "keyup" => {
                    let Some(code) = Self::script_key(arg) else {
                        log::warn!("input script: unknown key {arg}");
                        continue;
                    };
                    if verb != "keyup" {
                        self.on_key(event_loop, code, true, false);
                    }
                    if verb != "keydown" {
                        self.on_key(event_loop, code, false, false);
                    }
                }
                // `log pose`: where the bus and the camera are, and whether the bus has ground
                "log" if arg == "pose" => {
                    let bus = self.player.as_ref().map(|p| {
                        (
                            p.vehicle.position,
                            p.vehicle
                                .ground
                                .as_ref()
                                .and_then(|g| g(p.vehicle.position.x, p.vehicle.position.y)),
                        )
                    });
                    let tiles = self
                        .world
                        .as_ref()
                        .map(|w| w.loaded_tiles().len())
                        .unwrap_or(0);
                    log::info!("input script: bus at {:?} (ground {:?}), camera at {:?}, {tiles} tiles loaded", bus.map(|b| b.0), bus.and_then(|b| b.1), self.camera.as_ref().map(|c| c.position));
                    // the camera in the bus's own frame (x right, y forward, z up)
                    if let (Some(p), Some(c)) = (self.player.as_ref(), self.camera.as_ref()) {
                        let d = c.position - p.vehicle.position;
                        let h = p.vehicle.heading.to_radians();
                        let (fwd, right) = (glam::DVec2::new(h.sin(), h.cos()), glam::DVec2::new(h.cos(), -h.sin()));
                        log::info!("input script: view {} camera in the bus ({:.2}, {:.2}, {:.2}), on foot {:?}", self.view, d.truncate().dot(right), d.truncate().dot(fwd), d.z, self.on_foot.as_ref().map(|f| f.pos));
                    }
                }
                // `log mouse`: the mouse steering's state
                "log" if arg == "mouse" => {
                    log::info!(
                        "input script: mouse steering {} look {} menu {:?} paused {} focused {} steer {:.3}",
                        self.mouse_drive,
                        self.mouse_look,
                        self.game_menu,
                        self.paused,
                        self.window_focused,
                        self.mouse_steer.0
                    );
                }
                "log" => {
                    let v = self
                        .player
                        .as_ref()
                        .map(|p| (p.vehicle.var(arg), p.vehicle.str_var(arg)));
                    let names = describe::names(&self.args.root, &self.settings.language);
                    let shown = self
                        .hover
                        .as_deref()
                        .map(|h| names.control(h))
                        .or_else(|| self.hover_part.as_deref().map(|p| names.part(p)));
                    log::info!(
                        "input script: {arg} = {:?}  hover {:?} / {:?} shown as {:?}",
                        v,
                        self.hover,
                        self.hover_part,
                        shown
                    );
                }
                // `menu <what>`: a line of the game menu by its id (`menu remove`, `menu switch`),
                // `menu pick:<n>`: line n of the list open
                "menu" => {
                    if let Some(n) = arg.strip_prefix("pick:").and_then(|n| n.parse::<usize>().ok()) {
                        self.chooser_pick(n);
                    } else {
                        if self.game_menu.is_none() {
                            self.open_game_menu();
                        }
                        // (a line of the vehicle or world pages is done directly)
                        match self.game_menu_items().iter().position(|m| m.0 == arg) {
                            Some(k) => self.menu_choose(event_loop, k),
                            None => {
                                if !self.page_action(arg) {
                                    log::warn!("input script: no menu line {arg}");
                                }
                            }
                        }
                    }
                    let riders = self.humans.as_ref().map(|h| (h.people_in(crate::humans::BusId::Player), self.placed.iter().map(|q| h.people_in(crate::humans::BusId::Ai(crate::humans::placed_bus_id(q.uid)))).collect::<Vec<_>>()));
                    log::info!("input script: menu {arg}: player {:?}, on foot {:?}, placed {}, people in the bus / the placed ones {:?}", self.player.as_ref().map(|p| p.vehicle.position), self.on_foot.as_ref().map(|f| f.pos), self.placed.len(), riders);
                }
                // `shot <file>`: the window's own view into a PNG, drawn from the scene the
                // window is showing (the only way to see what the window path renders)
                "shot" => self.shot = Some(PathBuf::from(arg)),
                // `dumptex <folder>`: the player's display pictures as the window has them
                "dumptex" => {
                    if let Some(p) = self.player.as_ref() {
                        dump_display_textures(&p.vehicle, Path::new(arg));
                    }
                }
                _ => log::warn!("input script: unknown command {cmd}"),
            }
        }
    }

    /// What the cursor points at, for the HUD. Recomputed every frame: the head turns and
    /// the bus moves under a cursor that is standing still.
    /// Open the game menu: the simulation pauses (not in a LAN session, which runs on
    /// for the other players).
    pub(crate) fn open_game_menu(&mut self) {
        self.menu_prev_pause = self.paused;
        // the menu takes the keys, their key-ups too: what is held now is let go here, or a
        // steering key let go in the menu went on turning the wheel to full lock once the
        // menu closed (a throttle key went on accelerating, a door button stayed pressed)
        self.release_vehicle_keys();
        if self.lan.is_none() {
            self.paused = true;
        }
        self.game_menu = Some(0);
        self.menu_top = None;
        self.menu_kbd = true;
        self.menu_drag = None;
    }

    /// Let go of every key the vehicle holds: the keyboard's driving keys and pedals, the
    /// vehicle keys of `Inputs/keyboard.cfg` (their `<trigger>_off` fires) and the
    /// Shift+number door buttons.
    pub(crate) fn release_vehicle_keys(&mut self) {
        if let Some(p) = self.player.as_mut() {
            let held: Vec<_> = p.held_keys.keys().copied().collect();
            for scan in held {
                p.key(scan, 0, false);
            }
            p.axes.release_all();
            for fired in self.door_key_triggers.drain().map(|(_, g)| g).collect::<Vec<_>>() {
                p.door_key_off(&fired);
            }
        }
        self.door_key_triggers.clear();
    }

    /// The window lost the keyboard and the mouse (focus gone to another window, minimised,
    /// hidden, a phone sending the app to the background): no key-up or button-up comes for
    /// what is held now, so all of it is let go here - the vehicle's keys, a switch held
    /// with the mouse, looking round, the mouse's zoom - and the mouse steering holds the
    /// wheel and the brake where they are with its throttle off. Until the window has the
    /// focus again the mouse and the keyboard work nothing (see `input_away`). A game
    /// controller's axes stay theirs.
    pub(crate) fn input_lost(&mut self) {
        self.input_away = true;
        self.release_vehicle_keys();
        self.keys.clear();
        if let Some(p) = self.player.as_mut() {
            p.release();
        }
        self.dragging = false;
        self.buttons_held = (false, false);
        self.both_drag = None;
        self.mouse_look = false;
        self.steer_cursor = None;
        self.mouse_pedals.0 = 0.0;
    }

    /// The window has the focus again: the mouse steering eases from where the wheel stands
    /// to the cursor (as when it is switched on) instead of jumping there. A key still held
    /// from before counts only once it is pressed again.
    pub(crate) fn input_back(&mut self) {
        if !self.input_away {
            return;
        }
        self.input_away = false;
        self.mouse_steer = (self.player.as_ref().map(|p| p.vehicle.physics.controls.steering).unwrap_or(0.0), 1.0);
    }

    pub(crate) fn close_game_menu(&mut self) {
        if self.menu_edit_icao {
            if let Some(w)=self.window.as_ref(){w.set_ime_allowed(false);}
            self.menu_edit_icao=false; self.menu_edit=None;
        }
        self.game_menu = None;
        self.menu_top = None;
        self.paused = self.menu_prev_pause;
    }

    /// Show one of the menu's lists in the chooser (see `game_lists`).
    pub(crate) fn open_list(&mut self, kind: crate::game_lists::ListKind) {
        crate::game_lists::forget_page_titles();
        self.dropdown = None;
        self.admin_list = Some(crate::game_lists::items(self, &kind));
        self.list_kind = Some(kind);
        // (on its first line, not on a heading)
        self.chooser = Some(if self.is_heading(0) { self.chooser_next(0, 1) } else { 0 });
    }

    /// Line `k` of the list shown heads the lines under it (`game_lists::HEADING`).
    fn is_heading(&self, k: usize) -> bool {
        self.admin_list.as_ref().and_then(|l| l.get(k)).is_some_and(|l| l.1 == crate::game_lists::HEADING)
    }

    /// The line `step` lines on from `sel` (round the list; `n - 1` is one back), over the
    /// headings.
    fn chooser_next(&self, sel: usize, step: usize) -> usize {
        let n = self.admin_list.as_ref().unwrap_or(&self.vehicle_list).len().max(1);
        let mut k = sel;
        for _ in 0..n {
            k = (k + step) % n;
            if !self.is_heading(k) {
                break;
            }
        }
        k
    }

    /// Left or Right on line `k` of a list, or a click on the arrows round its value: its
    /// setting one step down (`-`) or up (`+`), see `game_lists::ADJUST`; other lines stay.
    pub(crate) fn chooser_adjust(&mut self, k: usize, dir: &str) {
        let Some(action) = self.admin_list.as_ref().and_then(|l| l.get(k)).and_then(|l| l.1.strip_suffix(crate::game_lists::ADJUST)).map(|a| format!("{a} {dir}")) else { return };
        // (run as a pick of the line, with the step in place of the mark)
        if let Some(l) = self.admin_list.as_mut().and_then(|l| l.get_mut(k)) {
            l.1 = action;
        }
        self.chooser_pick(k);
    }

    fn icao_edit_key(&mut self,code:KeyCode){
        match code{
            KeyCode::Escape=>{self.menu_edit=None;self.menu_edit_icao=false;if let Some(w)=self.window.as_ref(){w.set_ime_allowed(false);}},
            KeyCode::Backspace|KeyCode::Delete=>{if let Some(d)=self.menu_edit.as_mut(){d.pop();}},
            KeyCode::Enter|KeyCode::NumpadEnter=>{self.apply_icao_edit();return;},
            _=>{}
        }
        self.refresh_list();
    }
    pub(crate) fn icao_edit_text(&mut self,text:&str){
        if !self.menu_edit_icao{return}
        if let Some(d)=self.menu_edit.as_mut(){
            for c in text.chars().filter(|c|c.is_ascii_alphabetic()){
                if d.len()>=4{break} d.push(c.to_ascii_uppercase());
            }
        }
        self.refresh_list();
    }
    pub(crate) fn start_icao_edit(&mut self){
        self.menu_edit=Some(String::new()); self.menu_edit_icao=true;
        if let Some(w)=self.window.as_ref(){w.set_ime_allowed(true);}
    }
    pub(crate) fn apply_icao_edit(&mut self){
        let code=self.menu_edit.take().unwrap_or_default().trim().to_ascii_uppercase();
        self.menu_edit_icao=false;
        if let Some(w)=self.window.as_ref(){w.set_ime_allowed(false);}
        if code.len()==4&&code.chars().all(|c|c.is_ascii_alphabetic()){
            self.settings.metar_station=code.clone();
            crate::game_lists::remember_setting("metar_station",&code);
            self.metar_rx=None; self.metar_once=false; self.metar_next=0.0;
            self.service_msg=Some((format!("METAR source: {code}"),3.0));
        }else if !code.is_empty(){self.service_msg=Some(("ICAO must be exactly 4 letters".into(),3.0));}
        self.refresh_list();
    }

    fn time_edit_key(&mut self, code: KeyCode) {
        let digit = match code {
            KeyCode::Digit0 | KeyCode::Numpad0 => Some('0'),
            KeyCode::Digit1 | KeyCode::Numpad1 => Some('1'),
            KeyCode::Digit2 | KeyCode::Numpad2 => Some('2'),
            KeyCode::Digit3 | KeyCode::Numpad3 => Some('3'),
            KeyCode::Digit4 | KeyCode::Numpad4 => Some('4'),
            KeyCode::Digit5 | KeyCode::Numpad5 => Some('5'),
            KeyCode::Digit6 | KeyCode::Numpad6 => Some('6'),
            KeyCode::Digit7 | KeyCode::Numpad7 => Some('7'),
            KeyCode::Digit8 | KeyCode::Numpad8 => Some('8'),
            KeyCode::Digit9 | KeyCode::Numpad9 => Some('9'),
            _ => None,
        };
        match code {
            KeyCode::Escape => self.menu_edit = None,
            KeyCode::Backspace | KeyCode::Delete => {
                if let Some(d) = self.menu_edit.as_mut() {
                    d.pop();
                }
            }
            KeyCode::Enter | KeyCode::NumpadEnter => {
                self.apply_time_edit();
                return;
            }
            _ => {
                if let (Some(c), Some(d)) = (digit, self.menu_edit.as_mut()) {
                    if d.len() < 6 {
                        d.push(c);
                    }
                }
            }
        }
        self.refresh_list();
    }

    /// A key while a route number is typed in the destination list (#836). Printable
    /// text comes through `route_edit_text` so keyboard layouts and symbols are preserved;
    /// physical key codes remain a fallback for platforms that do not provide text.
    fn route_edit_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Escape => self.menu_edit = None,
            KeyCode::Backspace | KeyCode::Delete => {
                if let Some(t) = self.menu_edit.as_mut() {
                    t.pop();
                }
            }
            KeyCode::Enter | KeyCode::NumpadEnter => {
                if let Some(t) = self.menu_edit.take() {
                    crate::game_lists::set_route_by_hand(self, &t);
                    self.close_game_menu();
                }
                return;
            }
            _ => {
                if let (Some(c), Some(t)) = (route_char(code), self.menu_edit.as_mut()) {
                    if t.chars().count() < 8 {
                        t.push(c);
                    }
                }
            }
        }
        self.refresh_list();
    }

    /// Text entered in OMSI's free route-number field. It is intentionally not restricted
    /// to letters and digits: add-on displays use values such as `-10` and other symbols.
    pub(crate) fn route_edit_text(&mut self, text: &str) {
        if !matches!(self.list_kind, Some(crate::game_lists::ListKind::RouteNumbers)) || self.menu_edit.is_none() {
            return;
        }
        if let Some(t) = self.menu_edit.as_mut() {
            for c in text.chars().filter(|c| !c.is_control()) {
                if t.chars().count() >= 8 {
                    break;
                }
                t.push(c);
            }
        }
        self.refresh_list();
    }

    /// Set the clock to the time typed (digits: hh, hhmm or hhmmss; what is missing is 0).
    pub(crate) fn apply_time_edit(&mut self) {
        let Some(d) = self.menu_edit.take() else { return };
        if !d.is_empty() {
            let mut c = d.clone();
            while c.len() < 6 {
                c.push('0');
            }
            let n = |a: usize| c[a..a + 2].parse::<i64>().unwrap_or(0);
            let (h, m, sec) = (n(0), n(2), n(4));
            if h > 23 || m > 59 || sec > 59 {
                self.service_msg = Some((format!("{:02}:{:02}:{:02} is no time of day", h, m, sec), 3.0));
            } else if self.lan.as_ref().is_some_and(|l| l.role == omsi_net::Role::Client) {
                self.service_msg = Some(("In a LAN session the host sets the clock".into(), 3.0));
            } else if self.real_time_locked() {
                self.service_msg = Some(("The time cannot be changed while the real-time sync is on".into(), 3.0));
            } else {
                let t = self.clock.time;
                let day_start = t - t.rem_euclid(86400.0);
                self.shift_clock(day_start + (h * 3600 + m * 60 + sec) as f64 - t);
                self.service_msg = Some((format!("Clock: {:02}:{:02}:{:02}", h, m, sec), 3.0));
            }
        }
        self.refresh_list();
    }

    /// The open list made again from what it shows (a value changed), the chosen line kept.
    pub(crate) fn refresh_list(&mut self) {
        let Some(kind) = self.list_kind.clone() else { return };
        let keep = self.chooser;
        self.open_list(kind);
        if let (Some(k), Some(l)) = (keep, self.admin_list.as_ref()) {
            self.chooser = Some(k.min(l.len().saturating_sub(1)));
        }
    }

    /// Change the weather in force by hand: `f` changes a copy of it, which takes the place of
    /// the weather at once (a change on its way and the weather cycle stop: this is the
    /// weather now). The sky's clouds are made again when their type changed.
    pub(crate) fn edit_weather(&mut self,f:impl FnOnce(&mut omsi_content::weather::Weather)){
        if self.lan.as_ref().is_some_and(|l|l.role==omsi_net::Role::Client){
            self.service_msg=Some(("In a LAN session the host sets the weather".into(),3.0));return;
        }
        if self.metar_locked(){self.service_msg=Some(("The weather cannot be changed while the METAR sync is on".into(),3.0));return;}
        let mut w=self.weather.clone().unwrap_or_default();
        if w.precip.len()<5{w.precip.resize(5,0.0);}
        f(&mut w);
        let brightness=crate::weather_setup::custom_weather(self.args.weather.as_deref()).map(|c|c.brightness).unwrap_or(1.0);
        let custom=crate::weather_setup::CustomWeather::from_weather(&w,brightness,self.wetness);
        self.set_custom_weather(custom);
    }

    pub(crate) fn set_custom_weather(&mut self,mut custom:crate::weather_setup::CustomWeather){
        if self.lan.as_ref().is_some_and(|l|l.role==omsi_net::Role::Client){
            self.service_msg=Some(("In a LAN session the host sets the weather".into(),3.0));return;
        }
        if self.metar_locked(){self.service_msg=Some(("The weather cannot be changed while the METAR sync is on".into(),3.0));return;}
        custom.normalize();
        self.metar_rx=None;
        self.metar_once=false;
        let spec=custom.encode();
        let to=custom.to_weather();
        let clouds_changed=self.weather.as_ref().is_none_or(|w|w.clouds.0.trim()!=to.clouds.0.trim());
        self.args.weather=Some(spec.clone()); self.weather_blend=None; self.weather_cycle=None; self.wetness=custom.road_wetness;
        crate::scene::SNOW_WEATHER.store(to.snow,std::sync::atomic::Ordering::Relaxed);
        omsi_sim::host::set_ambient_weather(to.temp.0,to.temp.1);
        self.weather=Some(to);
        if clouds_changed{
            if let (Some(r),Some(scene))=(self.renderer.as_ref(),self.scene.as_mut()){
                crate::weather_setup::setup_sky(&self.args,r,scene,self.envir.as_ref(),self.weather.as_ref());
            }
        }
        self.follow_date();
        if let Some(l)=self.lan.as_mut().filter(|l|l.role==omsi_net::Role::Host){l.set_weather(&spec);}
        self.service_msg=Some(("Weather: Custom weather".into(),2.0));
    }

    /// A settings window (options, vehicle, world) is open.
    fn settings_list(&self) -> bool {
        use crate::game_lists::ListKind;
        self.chooser.is_some() && matches!(self.list_kind, Some(ListKind::Options(_) | ListKind::Vehicle(_) | ListKind::World(_)))
    }

    /// The open list is closed: back to the game menu.
    pub(crate) fn close_list(&mut self) {
        self.dropdown = None;
        if self.menu_edit_icao { if let Some(w)=self.window.as_ref(){w.set_ime_allowed(false);} }
        self.menu_edit_icao=false;
        self.menu_edit = None;
        self.chooser = None;
        self.admin_list = None;
        self.list_kind = None;
        self.menu_top = None;
    }

    /// Show page `i` of the open settings window.
    pub(crate) fn settings_tab(&mut self, i: usize) {
        use crate::game_lists::ListKind;
        let next = match self.list_kind {
            Some(ListKind::Options(_)) => ListKind::Options(i),
            Some(ListKind::Vehicle(_)) => ListKind::Vehicle(i),
            Some(ListKind::World(_)) => ListKind::World(i),
            _ => return,
        };
        self.menu_top = None;
        self.menu_edit = None;
        self.open_list(next);
    }

    /// The next (or previous) page of the open settings window, round the ends.
    fn settings_tab_step(&mut self, forward: bool) {
        let Some(kind) = self.list_kind.clone() else { return };
        let Some((titles, at)) = crate::game_lists::page_titles(self, &kind) else { return };
        let n = titles.len().max(1);
        self.settings_tab(if forward { (at + 1) % n } else { (at + n - 1) % n });
    }

    /// A click on the sidebar of a settings window: page `i`, or (the last box) the way back.
    pub(crate) fn settings_side_click(&mut self, i: usize) {
        let Some(kind) = self.list_kind.clone() else { return };
        let n = crate::game_lists::page_titles(self, &kind).map(|t| t.0.len()).unwrap_or(0);
        if i < n {
            self.settings_tab(i);
        } else {
            self.close_list();
        }
    }

    /// Change the value of line `k` of the open settings window as `mv` says.
    pub(crate) fn list_adjust(&mut self, k: usize, mv: crate::game_lists::Move) {
        use crate::game_lists::ListKind;
        let Some(kind) = self.list_kind.clone() else { return };
        if !matches!(kind, ListKind::Options(_) | ListKind::World(_)) {
            return;
        }
        let Some(action) = self.admin_list.as_ref().and_then(|l| l.get(k)).map(|x| x.1.clone()) else { return };
        let slider = crate::game_lists::is_slider(action.split(' ').next().unwrap_or(""));
        crate::game_lists::LIST_DIRTY.store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(next) = crate::game_lists::run_move(self, &kind, &action, mv) {
            // (a slider dragged sends the same value many times over: the list stays)
            if slider && !crate::game_lists::LIST_DIRTY.swap(false, std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            self.open_list(next);
            let last = self.admin_list.as_ref().map(|l| l.len().saturating_sub(1)).unwrap_or(0);
            self.chooser = Some(k.min(last));
        }
    }

    /// A click on the control of line `k` (a slider's track, a stepper), `fx` of the way
    /// along it from the left.
    /// True when the control is a slider (which the mouse button then holds: it follows the cursor).
    pub(crate) fn list_click(&mut self, k: usize, fx: f32) -> bool {
        use crate::game_lists::Move;
        let Some(action) = self.admin_list.as_ref().and_then(|l| l.get(k)).map(|x| x.1.clone()) else { return false };
        let verb = action.split(' ').next().unwrap_or("");
        let slider = crate::game_lists::is_slider(verb);
        let mv = if slider {
            Move::To(fx)
        } else if fx < 0.5 {
            Move::Dec
        } else {
            Move::Inc
        };
        self.list_adjust(k, mv);
        slider
    }


    /// A key while the vehicle chooser is open.
    fn chooser_key(&mut self, code: KeyCode) {
        if self.dropdown.is_some() {
            self.dropdown_key(code);
            return;
        }
        if self.menu_edit.is_some() {
            if self.menu_edit_icao {
                self.icao_edit_key(code);
            } else if matches!(self.list_kind, Some(crate::game_lists::ListKind::RouteNumbers)) {
                self.route_edit_key(code);
            } else {
                self.time_edit_key(code);
            }
            return;
        }
        let n = self.admin_list.as_ref().unwrap_or(&self.vehicle_list).len().max(1);
        let sel = self.chooser.unwrap_or(0);
        self.menu_top = None;
        match code {
            KeyCode::Escape => {
                if self.tours_list() {
                    self.open_list(crate::game_lists::ListKind::Lines);
                } else {
                    self.chooser = None;
                    self.admin_list = None;
                    self.list_kind = None;
                }
            }
            KeyCode::ArrowUp | KeyCode::KeyW => self.chooser = Some(self.chooser_next(sel, n - 1)),
            KeyCode::ArrowDown | KeyCode::KeyS => self.chooser = Some(self.chooser_next(sel, 1)),
            KeyCode::ArrowLeft | KeyCode::KeyA if self.settings_list() => self.list_adjust(sel, crate::game_lists::Move::Dec),
            KeyCode::ArrowRight | KeyCode::KeyD if self.settings_list() => self.list_adjust(sel, crate::game_lists::Move::Inc),
            KeyCode::Tab if self.settings_list() => {
                let back = self.keys.contains(&KeyCode::ShiftLeft) || self.keys.contains(&KeyCode::ShiftRight);
                self.settings_tab_step(!back);
            }
            // (the stop to start from: - and +)
            KeyCode::Minus | KeyCode::Slash | KeyCode::NumpadSubtract if self.tours_list() => self.tour_stop_step(false),
            KeyCode::Equal | KeyCode::BracketRight | KeyCode::NumpadAdd if self.tours_list() => self.tour_stop_step(true),
            // (the trip of the tour: the arrows go to the one leaving before or after, as OMSI's)
            KeyCode::ArrowLeft | KeyCode::KeyA if self.tours_list() => self.trip_step(false),
            KeyCode::ArrowRight | KeyCode::KeyD if self.tours_list() => self.trip_step(true),
            KeyCode::ArrowLeft | KeyCode::KeyA => self.chooser_adjust(sel, "-"),
            KeyCode::ArrowRight | KeyCode::KeyD => self.chooser_adjust(sel, "+"),
            KeyCode::PageUp if self.settings_list() => self.settings_tab_step(false),
            KeyCode::PageDown if self.settings_list() => self.settings_tab_step(true),
            KeyCode::PageUp => self.chooser = Some(sel.saturating_sub(15)).map(|k| if self.is_heading(k) { self.chooser_next(k, 1) } else { k }),
            KeyCode::PageDown => self.chooser = Some((sel + 15).min(n - 1)).map(|k| if self.is_heading(k) { self.chooser_next(k, 1) } else { k }),
            KeyCode::Enter | KeyCode::NumpadEnter | KeyCode::Space => self.chooser_pick(sel),
            _ => {}
        }
    }

    /// A key while a drop-down is open: the arrows choose, Enter takes, Esc closes it.
    fn dropdown_key(&mut self, code: KeyCode) {
        let Some(d) = self.dropdown.as_mut() else { return };
        let n = d.items.len().max(1);
        match code {
            KeyCode::Escape => {
                self.dropdown = None;
                return;
            }
            KeyCode::ArrowUp | KeyCode::KeyW => d.sel = (d.sel + n - 1) % n,
            KeyCode::ArrowDown | KeyCode::KeyS => d.sel = (d.sel + 1) % n,
            KeyCode::PageUp => d.sel = d.sel.saturating_sub(5),
            KeyCode::PageDown => d.sel = (d.sel + 5).min(n - 1),
            KeyCode::Home => d.sel = 0,
            KeyCode::End => d.sel = n - 1,
            KeyCode::Enter | KeyCode::NumpadEnter | KeyCode::Space => {
                let i = d.sel;
                self.dropdown_pick(i);
                return;
            }
            _ => return,
        }
        self.dd_reveal();
    }

    /// The drop-down's chosen entry is in view.
    fn dd_reveal(&mut self) {
        let rows = self.ui.as_ref().map(|u| u.dd_rows).unwrap_or(8).max(1);
        if let Some(d) = self.dropdown.as_mut() {
            if d.sel < d.top {
                d.top = d.sel;
            } else if d.sel >= d.top + rows {
                d.top = d.sel + 1 - rows;
            }
        }
    }

    /// Entry `i` of the open drop-down is taken: done, and the window's rows shown again
    /// with the new value.
    pub(crate) fn dropdown_pick(&mut self, i: usize) {
        let Some(d) = self.dropdown.take() else { return };
        let Some((_, action)) = d.items.get(i).cloned() else { return };
        crate::game_lists::dropdown_apply(self, &action);
        if let Some(kind) = self.list_kind.clone() {
            self.open_list(kind);
            let last = self.admin_list.as_ref().map(|l| l.len().saturating_sub(1)).unwrap_or(0);
            self.chooser = Some(d.row.min(last));
        }
    }

    /// A line's tours are open, with the stops of the tour chosen beside them.
    fn tours_list(&self) -> bool {
        self.chooser.is_some() && matches!(self.list_kind, Some(crate::game_lists::ListKind::Tours(..)))
    }

    /// The trip of the chosen tour leaving before (or after) the one chosen, as OMSI's
    /// timetable steps through the times of a tour (the tour itself stays).
    pub(crate) fn trip_step(&mut self, forward: bool) {
        let k = self.chooser.unwrap_or(0);
        let (Some((line, tour)), Some((_, _, trip, trips))) = (crate::game_lists::tour_at(self, k), crate::game_lists::tour_choice(self, k)) else { return };
        let to = if forward { (trip + 1).min(trips.saturating_sub(1)) } else { trip.saturating_sub(1) };
        if to != trip {
            self.pane_scroll = None;
            self.list_kind = Some(crate::game_lists::ListKind::Tours(line, Some((tour, 0, to))));
        }
    }

    /// The stop to start the chosen tour from, one on (or back).
    fn tour_stop_step(&mut self, forward: bool) {
        let k = self.chooser.unwrap_or(0);
        let (Some((line, tour)), Some((n, at, trip, _))) = (crate::game_lists::tour_at(self, k), crate::game_lists::tour_choice(self, k)) else { return };
        let to = if forward { (at + 1).min(n - 1) } else { at.saturating_sub(1) };
        self.pane_scroll = None;
        self.list_kind = Some(crate::game_lists::ListKind::Tours(line, Some((tour, to, trip))));
    }

    /// A click in the timetable beside the tours: stop `i` as the start, or (`usize::MAX`)
    /// the button that starts the trip; `usize::MAX - 1` / `- 2` the trip before / after.
    pub(crate) fn tour_pane_click(&mut self, i: usize) {
        let k = self.chooser.unwrap_or(0);
        // (the arrows beside the time: `usize::MAX - 1` the trip before, `- 2` the next)
        if i == usize::MAX - 1 || i == usize::MAX - 2 {
            self.trip_step(i == usize::MAX - 2);
            return;
        }
        let (Some((line, tour)), Some((n, at, trip, _))) = (crate::game_lists::tour_at(self, k), crate::game_lists::tour_choice(self, k)) else { return };
        if i < n {
            self.pane_scroll = None;
            self.list_kind = Some(crate::game_lists::ListKind::Tours(line, Some((tour, i, trip))));
            return;
        }
        crate::game_lists::start_duty_at(self, &line, &tour, trip, at);
        self.chooser = None;
        self.admin_list = None;
        self.list_kind = None;
        self.close_game_menu();
    }

    /// Place the chosen vehicle: in front of the camera in a free or map view, else beside
    /// the vehicle driven (OMSI puts a new vehicle where the map view points).
    pub(crate) fn chooser_pick(&mut self, k: usize) {
        // (a heading is no choice)
        if self.is_heading(k) {
            return;
        }
        // a row of a settings window that drops a list down (the weather preset, the clouds)
        if self.settings_list() {
            let id = self.admin_list.as_ref().and_then(|l| l.get(k)).map(|l| l.1.clone()).unwrap_or_default();
            if let Some(d) = crate::game_lists::dropdown_for(self, k, &id) {
                self.dropdown = Some(d);
                self.dd_reveal();
                return;
            }
        }
        self.chooser = None;
        // a list of the menu's (the administration, the options …): done, and the list
        // shown again - or the next one (a line's tours), or back to the menu
        if let Some(list) = self.admin_list.take() {
            let kind = self.list_kind.take().unwrap_or(crate::game_lists::ListKind::Admin);
            let Some((_, action)) = list.get(k).cloned() else { return };
            match crate::game_lists::run(self, &kind, &action) {
                Some(next) => {
                    let keep = next == kind;
                    if !keep {
                        self.menu_top = None;
                    }
                    self.open_list(next);
                    if keep {
                        self.chooser = Some(k.min(self.admin_list.as_ref().map(|l| l.len().saturating_sub(1)).unwrap_or(0)));
                    }
                }
                None if action != "back" && matches!(kind, crate::game_lists::ListKind::Tours(..) | crate::game_lists::ListKind::Numbers | crate::game_lists::ListKind::Destinations | crate::game_lists::ListKind::RouteNumbers | crate::game_lists::ListKind::Hofs | crate::game_lists::ListKind::Spots) => self.close_game_menu(),
                None => self.menu_top = None,
            }
            return;
        }
        // a vehicle of the list: its livery and depot file are asked for first
        self.menu_top = None;
        let Some((_, bus)) = self.vehicle_list.get(k).cloned() else { return };
        self.open_list(crate::game_lists::ListKind::PlaceLivery(bus));
    }

    /// Put the vehicle file `bus` down beside the camera or the bus driven, in `paint` (a
    /// scheme's name; None: at random) with the depot file `hof` (None: the map's).
    /// The driven vehicle read again from its files and put where it stands (#728).
    pub(crate) fn reload_driven_vehicle(&mut self) {
        let Some(p) = self.player.as_ref() else {
            self.service_msg = Some(("There is no vehicle to reload: you are on foot".into(), 3.0));
            return;
        };
        // (the file under its content root, as the vehicle lists name it: a whole path was
        // taken for one under the game's folder)
        let file = &p.vehicle.ty.def.path;
        let bus = omsi_cfg::content_roots().iter().chain(std::iter::once(&self.args.root)).find_map(|r| file.strip_prefix(r).ok()).unwrap_or(file).to_string_lossy().replace('\\', "/");
        let paint = p.vehicle.host.paint_scheme.flatten().and_then(|i| p.vehicle.ty.paint_schemes.get(i)).map(|s| s.name.clone());
        // (the depot file by its file name, as `find_hof` looks for it)
        let hof = p.vehicle.host.hof.as_ref().and_then(|h| h.path.file_stem().map(|s| s.to_string_lossy().to_string()).or_else(|| Some(h.name.clone())));
        let before = p.uid;
        self.swap_pending = true;
        self.place_vehicle(&bus, paint, hof);
        if let Some(p) = self.player.as_ref().filter(|p| p.uid != before) {
            let name = format!("{} {}", p.vehicle.ty.def.manufacturer, p.vehicle.ty.def.type_name);
            self.service_msg = Some((format!("Reloaded from its files: {}", name.trim()), 4.0));
        }
    }

    /// `q` (just spawned where the driven vehicle stands) becomes the one driven, and the
    /// one driven until now goes, with whoever rode in it (#728).
    fn replace_driven_vehicle(&mut self, q: Player) {
        let uid = q.uid;
        self.placed.insert(0, q);
        self.switch_vehicle();
        if !self.player.as_ref().is_some_and(|p| p.uid == uid) {
            return;
        }
        let Some(mut old) = self.placed.pop() else { return };
        if let (Some(a), Some(mut ss)) = (self.audio.as_ref(), old.sounds.take()) {
            ss.stop_all(a);
        }
        if let (Some(w), Some(r), Some(scene)) = (self.world.clone(), self.renderer.as_ref(), self.scene.as_mut()) {
            if let Some(h) = self.humans.as_mut() {
                h.evict(crate::humans::BusId::Ai(crate::humans::placed_bus_id(old.uid)), &w);
            }
            if let Some(mut d) = old.driver.take() {
                d.hide(r, scene);
            }
            w.release_vehicle(r, scene, old.render);
            for t in old.trailer_renders {
                w.release_vehicle(r, scene, t);
            }
        }
    }

    pub(crate) fn place_vehicle(&mut self, bus: &str, paint: Option<String>, hof: Option<String>) {
        // (in the driven vehicle's place, see `swap_pending`)
        let swap = std::mem::take(&mut self.swap_pending) && self.player.is_some();
        // (joined: the host or server offers only its buses - a plugin or input script asks by
        // file, past the menu, #1183)
        if crate::lan::joined_vehicles().is_some_and(|a| !crate::server::allows(&a, bus)) {
            self.service_msg = Some(("The server does not offer this bus".into(), 4.0));
            return;
        }
        let name = self.vehicle_list.iter().find(|v| v.1 == bus).map(|v| v.0.clone()).unwrap_or_else(|| bus.to_string());
        let bus = bus.to_string();
        let (Some(w), Some(r), Some(scene), Some(cam)) = (self.world.clone(), self.renderer.as_ref(), self.scene.as_mut(), self.camera.as_ref()) else { return };
        let (x, y, heading) = match (self.view.as_str(), self.player.as_ref()) {
            (_, Some(p)) if swap => (p.vehicle.position.x, p.vehicle.position.y, p.vehicle.heading),
            ("free", _) | (_, None) => {
                let f = cam.forward();
                let flat = glam::DVec2::new(f.x as f64, f.y as f64).normalize_or_zero();
                let at = cam.position.truncate() + flat * 15.0;
                (at.x, at.y, cam.yaw as f64)
            }
            (_, Some(p)) => {
                let h = p.vehicle.heading.to_radians();
                let right = glam::DVec2::new(h.cos(), -h.sin());
                let at = p.vehicle.position.truncate() + right * 5.0;
                (at.x, at.y, p.vehicle.heading)
            }
        };
        let one = Args {
            bus: Some(bus.clone()),
            spawn: Some(format!("{x},{y},{heading}")),
            situation_vars: Vec::new(),
            situation_strvars: Vec::new(),
            situation_others: Vec::new(),
            line: None,
            tour: None,
            trip: None,
            autostart: false,
            paint,
            hof: hof.or(self.args.hof.clone()),
            ..self.args.clone()
        };
        match spawn_player(&one, &w, r, scene) {
            Ok(Some(q)) if swap => {
                log::info!("{bus} takes the driven vehicle's place at ({x:.1}, {y:.1})");
                self.replace_driven_vehicle(q);
            }
            Ok(Some(q)) => {
                log::info!("placed {bus} at ({x:.1}, {y:.1})");
                let uid = q.uid;
                self.placed.push(q);
                // (then put down with the mouse, where the player wants it)
                self.begin_placing(uid, heading);
                let _ = name;
            }
            Ok(None) => {}
            Err(e) => self.service_msg = Some((format!("Could not place {name}: {e:#}"), 5.0)),
        }
    }

    /// Couple a standing vehicle to the back of the one driven: its front coupling within
    /// 2.5 m of the train's rear coupling, facing the same way.
    pub(crate) fn couple(&mut self) {
        let (Some(w), Some(r), Some(scene), Some(p)) = (self.world.clone(), self.renderer.as_ref(), self.scene.as_mut(), self.player.as_mut()) else { return };
        // the train's rear coupling in the world
        let rear = match p.vehicle.trailers.last() {
            Some(t) => {
                let c = if t.reversed { t.ty.def.coupling_front.as_ref() } else { t.ty.def.coupling_back.as_ref() };
                c.map(|c| (t.world_transform().transform_point3(glam::Vec3::from(c.pos)), t.heading))
            }
            None => p.vehicle.ty.def.coupling_back.as_ref().map(|c| (p.vehicle.world_transform().transform_point3(glam::Vec3::from(c.pos)), p.vehicle.heading)),
        };
        let Some((rear, rear_heading)) = rear else {
            self.service_msg = Some(("This vehicle has no coupling at its back".into(), 4.0));
            return;
        };
        let found = self.placed.iter().position(|q| {
            let Some(c) = q.vehicle.ty.def.coupling_front.as_ref() else { return false };
            let front = q.vehicle.world_transform().transform_point3(glam::Vec3::from(c.pos));
            let dh = ((q.vehicle.heading - rear_heading + 540.0).rem_euclid(360.0) - 180.0).abs();
            (front - rear).truncate().length() < 2.5 && dh < 35.0
        });
        let Some(k) = found else {
            self.service_msg = Some(("Nothing to couple: back up to a trailer's coupling (within 2.5 m, in line)".into(), 4.0));
            return;
        };
        let q = self.placed.remove(k);
        let ty = q.vehicle.ty.clone();
        if let (Some(a), Some(mut ss)) = (self.audio.as_ref(), q.sounds) {
            ss.stop_all(a);
        }
        w.release_vehicle(r, scene, q.render);
        for tr in q.trailer_renders {
            w.release_vehicle(r, scene, tr);
        }
        p.vehicle.attach_trailer_ex(ty.clone(), false);
        p.trailer_renders.push(w.add_vehicle_part(r, scene, &ty, None, &p.render));
        p.hand_coupled += 1;
        self.service_msg = Some((format!("Coupled: {} {}", ty.def.manufacturer, ty.def.type_name), 3.0));
    }

    /// Uncouple the last part coupled by hand: it stays where it is, a vehicle of its own.
    pub(crate) fn uncouple(&mut self) {
        let (Some(w), Some(r), Some(scene)) = (self.world.clone(), self.renderer.as_ref(), self.scene.as_mut()) else { return };
        let Some(p) = self.player.as_mut() else { return };
        if p.hand_coupled == 0 {
            self.service_msg = Some(("Nothing coupled by hand (an articulated bus's rear section stays)".into(), 4.0));
            return;
        }
        let Some(t) = p.vehicle.detach_last_trailer() else { return };
        if let Some(tr) = p.trailer_renders.pop() {
            w.release_vehicle(r, scene, tr);
        }
        p.hand_coupled -= 1;
        let one = Args {
            bus: Some(t.ty.def.path.to_string_lossy().to_string()),
            spawn: Some(format!("{},{},{},{}", t.position.x, t.position.y, t.heading, t.position.z)),
            situation_vars: Vec::new(),
            situation_strvars: Vec::new(),
            situation_others: Vec::new(),
            line: None,
            tour: None,
            trip: None,
            autostart: false,
            paint: None,
            ..self.args.clone()
        };
        match spawn_player(&one, &w, r, scene) {
            Ok(Some(q)) => {
                self.placed.push(q);
                self.service_msg = Some(("Uncoupled".into(), 3.0));
            }
            Ok(None) => {}
            Err(e) => log::warn!("uncoupled part: {e:#}"),
        }
    }

    /// A key while the game menu is open.
    /// The object editor on or off; on, it starts with the free camera where the view is.
    pub(crate) fn toggle_editor(&mut self) {
        // (in a LAN session the host edits the map for everybody: its edits go to the
        // others' games, a client's would stay its own)
        if self.editor.is_none() && self.lan.as_ref().map(|l| l.role == omsi_net::Role::Client).unwrap_or(false) {
            self.service_msg = Some(("In a LAN session only the host edits the map".into(), 3.0));
            return;
        }
        if self.editor.take().is_some() {
            self.editor_drag = false;
            self.service_msg = Some(("Object editor off (unsaved changes stay until the end of the session)".into(), 3.0));
            return;
        }
        let ed = crate::editor::Editor::default();
        let msg = self.world.as_ref().map(|w| ed.describe(w)).unwrap_or_default();
        self.editor = Some(ed);
        self.service_msg = Some((format!("{msg} - click picks, drag moves, wheel turns (Shift: height), Delete, C copy, V variant, Backspace undo, PgUp/PgDn/F ground, [ ] brush, Ctrl+S save, Esc leave"), 10.0));
    }

    /// A key while the object editor is on; true when it was the editor's.
    pub(crate) fn editor_key(&mut self, code: KeyCode) -> bool {
        let shift = self.keys.contains(&KeyCode::ShiftLeft) || self.keys.contains(&KeyCode::ShiftRight);
        let ctrl = self.keys.contains(&KeyCode::ControlLeft) || self.keys.contains(&KeyCode::ControlRight);
        let Some(cam) = self.camera.as_ref() else { return false };
        let (eye, fwd, yaw) = (cam.position, cam.forward(), cam.yaw as f64);
        let Some(action) = crate::editor::action_for(code, shift, ctrl, yaw) else { return false };
        let Some(world) = self.world.clone() else { return false };
        let msg = match action {
            crate::editor::Action::Leave => {
                self.toggle_editor();
                return true;
            }
            crate::editor::Action::Pick => {
                let ed = self.editor.as_mut().unwrap();
                ed.pick(&world, eye, fwd);
                ed.describe(&world)
            }
            crate::editor::Action::NextPick => {
                let ed = self.editor.as_mut().unwrap();
                ed.next_pick();
                ed.describe(&world)
            }
            crate::editor::Action::Save => {
                let content = crate::startup::content_dir();
                let ed = self.editor.as_ref().unwrap();
                match content.map(|c| ed.save(&world, &self.args.map, &c, &self.args.root)) {
                    Some(Ok(files)) if files.is_empty() => "Nothing to save".to_string(),
                    Some(Ok(files)) => format!("Saved {} tile(s) to the content folder ({})", files.len(), files.iter().filter_map(|f| f.file_name()).map(|n| n.to_string_lossy()).collect::<Vec<_>>().join(", ")),
                    Some(Err(e)) => format!("Not saved: {e}"),
                    None => "Not saved: no content folder".to_string(),
                }
            }
            a @ (crate::editor::Action::Ground(_) | crate::editor::Action::Flatten | crate::editor::Action::Brush(_)) => {
                let at = crate::editor::Editor::aim(&world, eye, fwd);
                let (msg, tiles) = self.editor.as_mut().unwrap().ground(&world, at, &a);
                if !tiles.is_empty() {
                    log::info!("map editor: ground of tiles {tiles:?} at {at:?}");
                    world.forget_staged(&tiles);
                    if let (Some(st), Some(r), Some(scene)) = (self.streamer.as_mut(), self.renderer.as_ref(), self.scene.as_mut()) {
                        st.reload(r, scene, Some(&tiles), self.audio.as_ref());
                    }
                }
                msg
            }
            a => {
                let (Some(r), Some(scene)) = (self.renderer.as_ref(), self.scene.as_mut()) else { return true };
                match self.editor.as_mut().unwrap().apply(&world, r, scene, &a) {
                    Some(m) => m,
                    None => "Pick an object first (Enter)".to_string(),
                }
            }
        };
        log::info!("object editor: {msg}");
        self.service_msg = Some((msg, 5.0));
        self.editor_broadcast(false);
        true
    }

    /// The host's edits to the other players' games (`all`: every edit of the session,
    /// sent again every ten seconds for the ones who joined since).
    pub(crate) fn editor_broadcast(&mut self, all: bool) {
        let (Some(ed), Some(w)) = (self.editor.as_ref(), self.world.as_ref()) else {
            if all {
                // (edits stay after the editor is left: sent from the world's list)
                if let (Some(w), Some(l)) = (self.world.as_ref(), self.lan.as_mut()) {
                    if l.role == omsi_net::Role::Host {
                        let lines = crate::editor::Editor::default().sync_lines(w, &self.args.root, true);
                        let ids: Vec<u32> = l.peers().map(|p| p.pose.id).filter(|id| *id != l.my_id).collect();
                        for line in &lines {
                            for id in &ids {
                                l.command(*id, line);
                            }
                        }
                    }
                }
            }
            return;
        };
        let Some(l) = self.lan.as_mut() else { return };
        if l.role != omsi_net::Role::Host {
            return;
        }
        let lines = ed.sync_lines(w, &self.args.root, all);
        let ids: Vec<u32> = l.peers().map(|p| p.pose.id).filter(|id| *id != l.my_id).collect();
        for line in &lines {
            if line.len() > omsi_net::MAX_CHAT {
                log::warn!("object editor: '{line}' is too long to send");
                continue;
            }
            for id in &ids {
                l.command(*id, line);
            }
        }
    }

    /// The mouse in the object editor: a click picks what is under the cursor (and starts
    /// dragging it), a drag moves it over the ground; true when the editor took it.
    pub(crate) fn editor_mouse(&mut self, pressed: bool) -> bool {
        if self.editor.is_none() {
            return false;
        }
        if !pressed {
            if self.editor_drag {
                self.editor_drag = false;
                self.editor_broadcast(false);
            }
            return true;
        }
        let (Some(cam), Some(s), Some(world)) = (self.camera.as_ref(), self.surface.as_ref(), self.world.clone()) else { return true };
        let (o, d) = self.world_cursor_ray(cam, (s.config.width, s.config.height));
        let ed = self.editor.as_mut().unwrap();
        // (the copy being edited stays the one dragged while it is under the cursor)
        let on_added = ed.editing_added.and_then(|k| ed.added.get(k)).map(|a| {
            let p = a.base + a.moved - o;
            let along = p.dot(d.as_dvec3());
            along > 0.0 && (p - d.as_dvec3() * along).length() < 2.5
        }).unwrap_or(false);
        if !on_added {
            ed.pick(&world, o, d);
        }
        self.editor_drag = on_added || ed.selected.is_some();
        let msg = ed.describe(&world);
        self.service_msg = Some((msg, 5.0));
        true
    }

    /// The cursor moved while an object is dragged.
    pub(crate) fn editor_drag_frame(&mut self) {
        if !self.editor_drag {
            return;
        }
        let (Some(cam), Some(s), Some(world)) = (self.camera.as_ref(), self.surface.as_ref(), self.world.clone()) else { return };
        let (o, d) = self.world_cursor_ray(cam, (s.config.width, s.config.height));
        let Some(hit) = crate::placing::ground_hit(&world, o, d.as_dvec3(), 400.0) else { return };
        let (Some(r), Some(scene), Some(ed)) = (self.renderer.as_ref(), self.scene.as_mut(), self.editor.as_mut()) else { return };
        if let Some(m) = ed.drag_to(&world, r, scene, hit) {
            self.service_msg = Some((m, 3.0));
        }
    }

    /// The wheel in the object editor: the object turns (5° a notch), with Shift it rises.
    pub(crate) fn editor_wheel(&mut self, amount: f32) -> bool {
        if self.editor.is_none() {
            return false;
        }
        let shift = self.keys.contains(&KeyCode::ShiftLeft) || self.keys.contains(&KeyCode::ShiftRight);
        let action = if shift { crate::editor::Action::Move(glam::DVec3::Z * 0.1 * amount as f64) } else { crate::editor::Action::Turn(5.0 * amount as f64) };
        let (Some(world), Some(r), Some(scene)) = (self.world.clone(), self.renderer.as_ref(), self.scene.as_mut()) else { return true };
        if let Some(m) = self.editor.as_mut().unwrap().apply(&world, r, scene, &action) {
            self.service_msg = Some((m, 3.0));
            self.editor_broadcast(false);
        }
        true
    }

    pub(crate) fn menu_key(&mut self, event_loop: &ActiveEventLoop, code: KeyCode) {
        self.menu_kbd = true;
        if self.chooser.is_some() {
            self.chooser_key(code);
            return;
        }
        let n = self.game_menu_items().len();
        let sel = self.game_menu.unwrap_or(0);
        let modified = self.keys.iter().any(|key| {
            matches!(*key, KeyCode::ControlLeft | KeyCode::ControlRight | KeyCode::AltLeft | KeyCode::AltRight | KeyCode::ShiftLeft | KeyCode::ShiftRight)
        });
        self.menu_top = None;
        match code {
            // P changes only the simulation state, even while a menu is open.
            KeyCode::KeyP if !modified => self.toggle_pause(),
            KeyCode::Escape => self.close_game_menu(),
            KeyCode::ArrowUp | KeyCode::KeyW => self.game_menu = Some(self.menu_step(sel, n, false)),
            KeyCode::ArrowDown | KeyCode::KeyS => self.game_menu = Some(self.menu_step(sel, n, true)),
            KeyCode::Enter | KeyCode::NumpadEnter | KeyCode::Space => self.menu_choose(event_loop, sel),
            _ => {}
        }
    }

    /// The open drop-down's scroll bar held with the mouse at height `y`: the first entry
    /// shown follows the thumb.
    pub(crate) fn drag_dropdown(&mut self, y: f32) {
        let (Some(grab), Some(ui)) = (self.dd_scroll_drag, self.ui.as_ref()) else { return };
        let Some((track, thumb)) = ui.dd_scroll else { return };
        let rows = ui.dd_rows.max(1);
        let Some(d) = self.dropdown.as_mut() else { return };
        d.top = dropdown_top_at(y - grab, track, thumb[3] - thumb[1], d.items.len(), rows);
    }

    /// The scroll bar of the timetable beside the tours held with the mouse at height `y`:
    /// the first stop shown follows the thumb.
    pub(crate) fn drag_pane(&mut self, y: f32) {
        let (Some(grab), Some(k)) = (self.pane_scroll_drag, self.chooser) else { return };
        let Some((track, thumb, n, fit)) = self.ui.as_ref().and_then(|u| u.menu_pane_scroll) else { return };
        self.pane_scroll = Some((k, dropdown_top_at(y - grab, track, thumb[3] - thumb[1], n, fit)));
    }

    /// The mouse wheel over the game menu: the chosen line moves (the menu scrolls with it),
    /// in a list the same; no wrapping round.
    pub(crate) fn menu_wheel(&mut self, amount: f32) {
        // (an open drop-down scrolls, not the window under it)
        if self.dropdown.is_some() {
            self.wheel_acc += amount;
            let steps = self.wheel_acc.trunc() as i64;
            if steps == 0 {
                return;
            }
            self.wheel_acc -= steps as f32;
            let rows = self.ui.as_ref().map(|u| u.dd_rows).unwrap_or(8);
            if let Some(d) = self.dropdown.as_mut() {
                let max = d.items.len().saturating_sub(rows) as i64;
                d.top = (d.top as i64 - steps).clamp(0, max) as usize;
            }
            return;
        }
        self.wheel_acc += amount;
        let steps = self.wheel_acc.trunc() as i64;
        if steps == 0 {
            return;
        }
        self.wheel_acc -= steps as f32;
        // the wheel over the timetable beside the tours scrolls its stops
        if let (Some(u), Some(k)) = (self.ui.as_ref(), self.chooser) {
            let (x, y) = self.cursor;
            if u.menu_pane_box.is_some_and(|r| x >= r[0] && x <= r[2] && y >= r[1] && y <= r[3]) {
                let first = (u.menu_pane_start as i64 - steps).max(0) as usize;
                self.pane_scroll = Some((k, first));
                return;
            }
        }
        // the list scrolls under the mouse; what is chosen stays chosen (the wheel used to
        // walk the highlight up and down the lines)
        let n = self.menu_len() as f32;
        let (start, rows) = self.ui.as_ref().map(|u| (u.menu_start as f32, u.menu_rows as f32)).unwrap_or((0.0, n));
        let top = (self.menu_top.unwrap_or(start) - steps as f32).clamp(0.0, (n - rows).max(0.0));
        self.menu_top = Some(top);
    }

    /// How many lines the menu shows now (the chooser's list, else the game menu's).
    pub(crate) fn menu_len(&self) -> usize {
        match self.chooser {
            Some(_) => self.admin_list.as_ref().unwrap_or(&self.vehicle_list).len(),
            None => self.game_menu_items().len(),
        }
    }

    /// Do what line `k` of the game menu says.
    pub(crate) fn menu_choose(&mut self, event_loop: &ActiveEventLoop, k: usize) {
        // (a click in an open list keeps the scroll where it is: no jump to the line)
        self.menu_top = if self.chooser.is_some() { self.ui.as_ref().map(|u| u.menu_start as f32) } else { None };
        if self.chooser.is_some() {
            self.chooser_pick(k);
            return;
        }
        // (a greyed-out line does nothing)
        if self.menu_item_off(k) {
            return;
        }
        let Some(id) = self.game_menu_items().get(k).map(|m| m.0) else { return };
        match id {
            "resume" => self.close_game_menu(),
            "options" => self.open_list(crate::game_lists::ListKind::Options(0)),
            "camera" => {
                let tab = crate::game_lists::options_tab(self, "Camera");
                self.open_list(crate::game_lists::ListKind::Options(tab));
            }
            "vehicle" => self.open_list(crate::game_lists::ListKind::Vehicle(0)),
            "world" => self.open_list(crate::game_lists::ListKind::World(0)),
            "copycode" => {
                self.close_game_menu();
                self.copy_server_code();
            }
            "admin" => self.open_list(crate::game_lists::ListKind::Admin),
            "duty" => self.open_list(crate::game_lists::ListKind::Lines),
            "map" => {
                self.close_game_menu();
                if let Some(n) = self.navigator.as_mut() {
                    if !n.map_open() {
                        n.toggle_map();
                    }
                }
            }
            "save" => {
                self.quick_save();
                self.close_game_menu();
            }
            "saveslot" => {
                self.save_slot();
                self.close_game_menu();
            }
            "shot" => {
                self.close_game_menu();
                self.take_screenshot();
            }
            // the route ends here: free drive, as the list of lines has it
            "endduty" => {
                self.duty = None;
                self.service_msg = Some(("Free drive: no duty".into(), 4.0));
                self.close_game_menu();
            }
            "tobus" => {
                self.close_game_menu();
                self.back_to_bus();
            }
            "load" => {
                self.game_menu = None;
                if self.load_quicksave() {
                    self.finish_session();
                    crate::platform::exit(event_loop);
                }
            }
            "quit" => {
                self.game_menu = None;
                self.finish_session();
                crate::platform::exit(event_loop);
            }
            // (the rest are the lines of the vehicle and world pages)
            other => {
                self.page_action(other);
            }
        }
    }

    /// The actions of the vehicle and world pages (and of what the plugins and the input
    /// script ask of the menu by name). False when `id` is none of them.
    pub(crate) fn page_action(&mut self, id: &str) -> bool {
        match id {
            "swap" | "place" => {
                // (a plain "Place a vehicle" puts one beside; "Swap" in the driven one's place)
                self.swap_pending = id == "swap" && self.player.is_some();
                if self.vehicle_list.is_empty() {
                    let menu = crate::menu::Menu::new(&self.args.root, &self.args.map);
                    self.vehicle_meta = menu.vehicles.iter().zip(menu.vehicle_meta).map(|(v, meta)| (v.1.clone(), meta)).collect();
                    self.vehicle_list = menu.vehicles;
                    // (alphabetical)
                    self.vehicle_list.sort_by_key(|v| v.0.to_lowercase());
                    crate::mt::protect(self.vehicle_list.iter().map(|v| v.0.as_str()));
                }
                // (joined: only the buses the host or server offers, #1183)
                let offered = crate::lan::joined_vehicles();
                if let Some(a) = offered.as_ref() {
                    self.vehicle_list.retain(|v| crate::server::allows(a, &v.1));
                }
                if self.vehicle_list.is_empty() && offered.is_some() {
                    self.service_msg = Some(("The server offers none of the buses installed here".into(), 4.0));
                } else if self.vehicle_list.is_empty() {
                    self.service_msg = Some(("No vehicles found".into(), 3.0));
                } else {
                    // (as the launcher's bus step: the manufacturer, then the type)
                    self.open_list(crate::game_lists::ListKind::PlaceMaker);
                }
            }
            "couple" => {
                self.close_game_menu();
                self.couple();
            }
            "uncouple" => {
                self.close_game_menu();
                self.uncouple();
            }
            "tobus" => {
                self.close_game_menu();
                self.back_to_bus();
            }
            "remove" => {
                self.close_game_menu();
                self.remove_driven_vehicle();
            }
            "reload" => {
                self.close_game_menu();
                self.reload_driven_vehicle();
            }
            "clearplaced" => {
                self.close_game_menu();
                self.remove_placed_vehicles();
            }
            "getout" => {
                self.close_game_menu();
                self.get_up();
            }
            "reset" => {
                self.close_game_menu();
                if let Some(p) = self.player.as_ref() {
                    let (at, heading) = (p.vehicle.position, p.vehicle.heading);
                    crate::admin::teleport(self, at, heading);
                    self.service_msg = Some(("The vehicle stands on its wheels again".into(), 3.0));
                }
            }
            "teleport" => {
                self.close_game_menu();
                if let Some(n) = self.navigator.as_mut() {
                    if !n.map_open() {
                        n.toggle_map();
                    }
                    self.teleport_pick = true;
                    self.service_msg = Some(("Click a street on the map: the bus is put there".into(), 6.0));
                }
            }
            "driver" => self.open_list(crate::game_lists::ListKind::Drivers),
            "number" => self.open_list(crate::game_lists::ListKind::Numbers),
            "dest" => self.open_list(crate::game_lists::ListKind::Destinations),
            "hof" => self.open_list(crate::game_lists::ListKind::Hofs),
            "tplist" => self.open_list(crate::game_lists::ListKind::Spots),
            "editor" => {
                self.close_game_menu();
                self.toggle_editor();
            }
            "timetable" => {
                self.timetable = !self.timetable;
                self.close_game_menu();
            }
            "info" => {
                self.info_bar = !self.info_bar;
                self.close_game_menu();
            }
            "refuel" | "wash" | "repair" => {
                self.close_game_menu();
                self.run_service(id);
            }
            "weather" => {
                self.close_game_menu();
                self.next_weather();
            }
            "metar_once" => self.load_metar_once(),
            "metar_refresh" => self.refresh_metar_now(),
            "weather_custom" => self.current_weather_as_custom(),
            "switch" => {
                self.close_game_menu();
                self.switch_vehicle();
            }
            "later" | "earlier" | "later10" | "earlier10" => {
                self.close_game_menu();
                if self.lan.as_ref().map(|l| l.role == omsi_net::Role::Client).unwrap_or(false) {
                    self.service_msg = Some(("In a LAN session the host sets the clock".into(), 3.0));
                } else {
                    self.shift_clock(match id {
                        "later" => 3600.0,
                        "earlier" => -3600.0,
                        "later10" => 600.0,
                        _ => -600.0,
                    });
                }
            }
            _ => return false,
        }
        true
    }

    /// OMSI's weather dialog, the short way: the next weather of the Weather folder, in
    /// force at once (the roads keep their wetness until the rain changes it).
    /// How fast the clock runs: the session's in LAN play (the host's, which its time speed
    /// setting or its administration set), else the settings'.
    pub(crate) fn time_speed(&self) -> f64 {
        if self.real_time_locked() {
            return 1.0;
        }
        match self.lan.as_ref() {
            Some(l) => l.clock_speed,
            None => self.settings.time_speed.clamp(1.0, 30.0),
        }
    }

    pub(crate) fn next_weather(&mut self) {
        self.step_weather();
    }

    /// The next (`dir` 1) or previous (-1) weather of the Weather folder, round the ends.
    pub(crate) fn step_weather(&mut self) {
        if self.lan.as_ref().map(|l| l.role == omsi_net::Role::Client).unwrap_or(false) {
            self.service_msg = Some(("In a LAN session the host sets the weather".into(), 3.0));
            return;
        }
        let mut files: Vec<String> = omsi_cfg::read_dir_merged("Weather")
            .into_iter()
            .filter(|p| p.extension().map(|e| e.eq_ignore_ascii_case("owt")).unwrap_or(false))
            .filter_map(|p| p.file_name().map(|n| format!("Weather/{}", n.to_string_lossy())))
            .collect();
        files.sort();
        files.dedup();
        if files.is_empty() {
            return;
        }
        let cur = self.args.weather.clone().unwrap_or_default().replace('\\', "/").to_ascii_lowercase();
        let i = files.iter().position(|f| f.to_ascii_lowercase() == cur).map(|i| (i + 1) % files.len()).unwrap_or(0);
        self.change_weather(Some(files[i].clone()), true, 1.0);
    }

    /// Go over to weather `file` (None: the map's default) in `secs` of the day (see
    /// `weather_cycle`); a host tells the others (`share`), who come over to it the same way.
    /// The player's own choice comes at once, as in Omsi.exe (the weather dialog loads the
    /// .owt and applies it straight away, 0x6828e0 -> 0x754c80); the cycle blends it in.
    pub(crate) fn change_weather(&mut self, file: Option<String>, share: bool, secs: f32) {
        if self.metar_locked() {
            self.service_msg = Some(("The weather cannot be changed while the METAR sync is on".into(), 3.0));
            return;
        }
        self.metar_rx = None;
        self.metar_once = false;
        let from = self.weather.clone().unwrap_or_default();
        self.args.weather = file.clone();
        let to = load_weather(&self.args);
        let name = to.name.clone();
        self.weather_blend = Some(crate::weather_cycle::Blend::new(from, to, secs));
        if share {
            // (a host: the others take it up with its next clock message)
            if let (Some(l), Some(f)) = (self.lan.as_mut(), file.as_ref()) {
                l.set_weather(f);
            }
        }
        log::info!("weather: going over to {file:?} ({name})");
        self.service_msg = Some((format!("Weather: {name}"), 4.0));
    }

    /// The weather this frame: a change coming in, and the cycle's next one (`secs` of the
    /// day went by; in LAN play only the host's cycle runs, the others follow it).
    pub(crate) fn tick_weather(&mut self, secs: f32) {
        if let Some(b) = self.weather_blend.as_mut() {
            let (w, clouds_changed, done) = b.step(secs);
            self.weather = Some(w);
            if done {
                self.weather_blend = None;
            }
            if clouds_changed {
                if let (Some(r), Some(scene)) = (self.renderer.as_ref(), self.scene.as_mut()) {
                    crate::weather_setup::setup_sky(&self.args, r, scene, self.envir.as_ref(), self.weather.as_ref());
                }
            }
        }
        if let Some(w) = self.weather.as_ref() {
            crate::weather_setup::cloud_drift_step(&mut self.cloud_drift, w, secs as f64);
        }
        let follows = self.lan.as_ref().is_some_and(|l| l.role == omsi_net::Role::Client);
        if follows || self.weather_blend.is_some() {
            return;
        }
        if self.metar_locked() {
            return;
        }
        let Some(c) = self.weather_cycle.as_mut() else { return };
        c.next_in -= secs as f64;
        if c.next_in > 0.0 {
            return;
        }
        c.next_in = c.interval();
        let r = c.rand();
        let all = crate::weather_cycle::installed();
        let now = self.weather.clone().unwrap_or_default();
        let now_file = self.args.weather.clone().unwrap_or_default();
        if let Some(next) = crate::weather_cycle::pick(&all, &now, &now_file, self.clock.day_month().1, r) {
            self.change_weather(Some(next), true, 240.0);
        }
    }

    /// Drive another of the vehicles standing in the world (a situation's): the one driven
    /// now stays where it is with everything as it was, and its sounds go to the next one.
    pub(crate) fn switch_vehicle(&mut self) {
        if self.placed.is_empty() {
            self.service_msg = Some(("There is no other vehicle to drive".into(), 3.0));
            return;
        }
        let Some(mut now) = self.player.take() else {
            // on foot without a bus: the first placed one's wheel
            self.take_placed(0);
            return;
        };
        if let (Some(a), Some(mut ss)) = (self.audio.as_ref(), now.sounds.take()) {
            ss.stop_all(a);
        }
        let mut next = self.placed.remove(0);
        if let Some(a) = self.audio.as_ref() {
            next.load_sounds(a);
        }
        // the riders stay in the bus left; the people know the new one's cabin
        if let Some(h) = self.humans.as_mut() {
            h.player_bus_swapped(now.uid, next.uid, &mut next.vehicle);
        }
        next.vehicle.host.auto_clutch = if self.settings.auto_clutch { 1.0 } else { 0.0 };
        self.placed.push(now);
        let name = format!("{} {}", next.vehicle.ty.def.manufacturer, next.vehicle.ty.def.type_name);
        if let Some(cam) = self.camera.as_ref() {
            self.camera = Some(next.camera(&self.view, cam));
        }
        self.player = Some(next);
        self.look = (0.0, 0.0);
        self.service_msg = Some((format!("Now driving: {}", name.trim()), 4.0));
    }

    /// The weather follows the METAR report and cannot be changed (the `metar_sync` setting).
    /// In a LAN session as a client the host's weather counts: the host syncs, not us.
    pub(crate) fn metar_locked(&self) -> bool {
        self.settings.metar_sync && !self.lan.as_ref().is_some_and(|l| l.role == omsi_net::Role::Client)
    }

    /// The airport whose report the sync follows: the one chosen, else the one of the weather
    /// in force, else the one nearest the map.
    pub(crate) fn metar_station(&self) -> String {
        if !self.settings.metar_station.is_empty() {
            return self.settings.metar_station.to_ascii_uppercase();
        }
        match self.args.weather.as_deref().and_then(|w| w.strip_prefix("metar:")) {
            Some(code) if !code.trim().is_empty() => code.trim().to_ascii_uppercase(),
            _ => crate::launcher::drive::nearest_airport(&self.args.root.to_string_lossy(), &self.args.map),
        }
    }

    /// Fetch the selected station once, without turning the ten-minute METAR sync on.
    pub(crate) fn load_metar_once(&mut self) {
        if self.lan.as_ref().is_some_and(|l| l.role == omsi_net::Role::Client) {
            self.service_msg=Some(("In a LAN session the host sets the weather".into(),3.0));
            return;
        }
        let icao=self.metar_station();
        self.metar_rx=None;
        self.metar_once=true;
        let (tx,rx)=std::sync::mpsc::channel();
        self.metar_rx=Some(rx);
        std::thread::spawn(move||{let _=tx.send(crate::weather_setup::try_metar(&icao));});
        self.service_msg=Some((format!("Weather: loading METAR for {}",self.metar_station()),4.0));
    }

    /// Ask the continuous METAR sync to fetch its selected station immediately.
    pub(crate) fn refresh_metar_now(&mut self) {
        if !self.metar_locked() {
            self.load_metar_once();
            return;
        }
        self.metar_rx=None;
        self.metar_once=false;
        self.metar_next=0.0;
        self.service_msg=Some((format!("Weather: refreshing METAR for {}",self.metar_station()),4.0));
    }

    /// Freeze the weather currently in force into an editable custom state.
    pub(crate) fn current_weather_as_custom(&mut self) {
        if self.metar_locked() {
            self.service_msg=Some(("Turn METAR sync off before editing its current weather".into(),3.0));
            return;
        }
        let Some(w)=self.weather.as_ref() else{return};
        let brightness=crate::weather_setup::custom_weather(self.args.weather.as_deref()).map(|c|c.brightness).unwrap_or(1.0);
        let c=crate::weather_setup::CustomWeather::from_weather(w,brightness,self.wetness);
        self.set_custom_weather(c);
    }

    /// The METAR sync: with it on, the report is downloaded in the background (at once, then
    /// every ten minutes) and the weather goes over to it; `dt` is real seconds.
    pub(crate) fn tick_metar(&mut self, dt: f32) {
        self.share_start_metar();
        if let Some(rx)=self.metar_rx.as_ref(){
            match rx.try_recv(){
                Ok(report)=>{
                    let once=self.metar_once;
                    self.metar_rx=None;
                    self.metar_once=false;
                    match report{
                        Some(w)=>self.apply_metar(w),
                        None=>self.service_msg=Some(("Weather: no METAR report could be loaded".into(),4.0)),
                    }
                    if once{return;}
                }
                Err(std::sync::mpsc::TryRecvError::Empty)=>return,
                Err(std::sync::mpsc::TryRecvError::Disconnected)=>{
                    let once=self.metar_once;
                    self.metar_rx=None;
                    self.metar_once=false;
                    if once{self.service_msg=Some(("Weather: METAR request failed".into(),4.0));return;}
                }
            }
        }
        if !self.metar_locked() {
            self.metar_next = 0.0;
            return;
        }
        self.metar_next -= dt as f64;
        if self.metar_next > 0.0 {
            return;
        }
        // (a failed download is tried again in a minute)
        self.metar_next = 60.0;
        let icao = self.metar_station();
        let (tx, rx) = std::sync::mpsc::channel();
        self.metar_rx = Some(rx);
        self.metar_once = false;
        std::thread::spawn(move || {
            let _ = tx.send(crate::weather_setup::try_metar(&icao));
        });
    }

    /// A host that started on `metar:<ICAO>` tells the players the report's values as soon as
    /// they are there (they cannot download it by the station's name: only the host syncs).
    fn share_start_metar(&mut self) {
        let Some(l) = self.lan.as_mut().filter(|l| l.role == omsi_net::Role::Host) else { return };
        if !l.weather().to_ascii_lowercase().starts_with("metar:") {
            return;
        }
        if let Some(wire) = self.weather.as_ref().and_then(crate::weather_setup::report_wire) {
            l.set_weather(&wire);
        }
    }

    /// Go over to the weather of a METAR report that came in.
    fn apply_metar(&mut self, to: omsi_content::weather::Weather) {
        self.metar_next = 600.0;
        let file = to.path.to_string_lossy().to_string();
        // (what the players are told: the report's values, which they make the weather from)
        let wire = crate::weather_setup::report_wire(&to).unwrap_or_else(|| file.clone());
        let name = to.name.clone();
        let from = self.weather.clone().unwrap_or_default();
        crate::scene::SNOW_WEATHER.store(to.snow, std::sync::atomic::Ordering::Relaxed);
        omsi_sim::host::set_ambient_weather(to.temp.0, to.temp.1);
        self.args.weather = Some(file.clone());
        self.weather_cycle = None;
        self.weather_blend = Some(crate::weather_cycle::Blend::new(from, to, 60.0));
        if let Some(l) = self.lan.as_mut().filter(|l| l.role == omsi_net::Role::Host) {
            l.set_weather(&wire);
        }
        log::info!("weather: METAR sync, going over to {file} ({name})");
        self.service_msg = Some((format!("Weather: {name}"), 4.0));
    }

    /// The clock follows the real time and cannot be changed (the `time_sync` setting). In a
    /// LAN session as a client the host's clock counts: the host or the server syncs, not us.
    pub(crate) fn real_time_locked(&self) -> bool {
        self.settings.time_sync && !self.lan.as_ref().is_some_and(|l| l.role == omsi_net::Role::Client)
    }

    /// With the real-time sync on: hold the clock to this device's date and time (a second
    /// off at most; a bigger gap - the game was paused - is jumped, the traffic's clock with it).
    pub(crate) fn sync_real_time(&mut self) {
        if !self.real_time_locked() {
            return;
        }
        let Some(real) = crate::real_time::clock_now(&self.clock) else { return };
        let gap = crate::real_time::gap(&self.clock, &real);
        if gap.abs() < 0.25 {
            return;
        }
        self.clock.year = real.year;
        self.clock.day_of_year = real.day_of_year;
        self.clock.time = real.time;
        if let Some(tr) = self.traffic.as_mut() {
            tr.day_time += gap;
        }
        if let Some(p) = self.player.as_mut() {
            p.vehicle.host.clock = self.clock.clone();
        }
    }

    /// Move the clock by `secs` (the traffic's clock with it), as OMSI's time dialog does.
    pub(crate) fn shift_clock(&mut self, secs: f64) {
        if self.real_time_locked() {
            self.service_msg = Some(("The time cannot be changed while the real-time sync is on".into(), 3.0));
            return;
        }
        let mut t = self.clock.time + secs;
        while t < 0.0 {
            t += 86400.0;
            self.clock.day_of_year = if self.clock.day_of_year > 1 { self.clock.day_of_year - 1 } else { omsi_sim::clock::days_in_year(self.clock.year - 1) };
        }
        while t >= 86400.0 {
            t -= 86400.0;
            self.clock.day_of_year = self.clock.day_of_year % omsi_sim::clock::days_in_year(self.clock.year) + 1;
        }
        self.clock.time = t;
        if let Some(tr) = self.traffic.as_mut() {
            tr.day_time += secs;
        }
        if let Some(p) = self.player.as_mut() {
            p.vehicle.host.clock = self.clock.clone();
        }
        let h = (t / 3600.0) as u32;
        self.service_msg = Some((format!("Clock: {h:02}:{:02}", ((t / 60.0) as u32) % 60), 3.0));
    }

    /// Start the game again on the quicksave (`Situations/quicksave.osn` of the content
    /// folder): a fresh process, as OMSI loads a situation into a fresh world. False when
    /// there is none.
    pub(crate) fn load_quicksave(&mut self) -> bool {
        let dir = crate::startup::content_dir().unwrap_or_else(|| self.args.root.clone()).join("Situations");
        let file = dir.join("quicksave.osn");
        if !file.exists() {
            self.service_msg = Some(("No quicksave yet (Ctrl+S saves one)".into(), 4.0));
            return false;
        }
        let Ok(exe) = std::env::current_exe() else { return false };
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("--root").arg(&self.args.root).arg("--no-menu").arg("--situation").arg(&file);
        // (the duty typed by itself goes on being typed: `--autostart` with a situation)
        if self.player.as_ref().is_some_and(|p| p.duty_typed) {
            cmd.arg("--autostart");
        }
        match cmd.spawn() {
            Ok(_) => {
                log::info!("loading {} in a new game", file.display());
                true
            }
            Err(e) => {
                self.service_msg = Some((format!("Could not start the game again: {e}"), 5.0));
                false
            }
        }
    }

    /// One of the depot services of the game menu: "refuel", "wash" or "repair".
    pub(crate) fn run_service(&mut self, kind: &str) {
        let Some(w) = self.world.clone() else { return };
        let Some(p) = self.player.as_mut() else { return };
        let one = Args {
            refuel: kind == "refuel",
            wash: kind == "wash",
            repair: kind == "repair",
            ..self.args.clone()
        };
        let at_station = at_petrol_station(&w, &p.vehicle);
        let mut clock = self.clock.clone();
        let msg = run_services(&one, &mut p.vehicle, &mut clock, w.global.repair_time_min, at_station);
        while clock.time >= 86400.0 {
            clock.time -= 86400.0;
            clock.day_of_year = clock.day_of_year % omsi_sim::clock::days_in_year(clock.year) + 1;
        }
        if !self.settings.time_sync || self.lan.as_ref().is_some_and(|l| l.role == omsi_net::Role::Client) {
            self.clock = clock;
        }
        p.vehicle.host.clock = self.clock.clone();
        for line in &msg {
            log::info!("{line}");
        }
        if let Some(line) = msg.into_iter().next() {
            self.service_msg = Some((line, 6.0));
        }
    }

    #[cfg(windows)]
    fn vr_action(&mut self, name: &str) -> bool {
        if name == "vr_toggle_mode" {
            self.vr_zoom_active = false;
            if !self.settings.vr_requested() { return false; }
            if self.vr.is_some() {
                self.vr = None;
                self.service_msg = Some(("Desktop mode".into(), 2.0));
            } else if let Some(renderer) = self.renderer.as_ref() {
                match crate::openxr::Vr::new(renderer, self.settings.vr_scale,
                                             self.settings.vr_desktop_mirror) {
                    Ok(vr) => {
                        self.vr = Some(vr);
                        self.service_msg = Some(("VR mode".into(), 2.0));
                    }
                    Err(e) => {
                        log::error!("OpenXR could not restart: {e:#}");
                        self.service_msg = Some((format!("{}: {e}", omsi_ui::tr("Could not start VR")), 5.0));
                    }
                }
            }
            self.hover_key = None;
            if matches!(self.list_kind, Some(crate::game_lists::ListKind::Options(_))) {
                self.refresh_list();
            }
            return true;
        }
        if self.vr.is_none() { return false; }
        match name {
            "vr_recenter" => {
                self.vr.as_mut().unwrap().recenter();
                self.look = (0.0, 0.0);
                self.service_msg = Some(("VR view recentered".into(), 2.0));
            }
            "vr_toggle_desktop_mirror" => {
                let visible = self.vr.as_mut().unwrap().toggle_desktop_mirror();
                self.settings.vr_desktop_mirror = visible;
                self.service_msg = Some((if visible { "Desktop VR mirror on" }
                                         else { "Desktop VR mirror off" }.into(), 2.0));
            }
            "vr_toggle_navigator" => self.vr_nav_adjust("enabled", 1.0),
            "vr_position_navigator" => self.start_vr_nav_edit(),
            _ => return false,
        }
        true
    }

    /// One of OMSI's global key actions; false when it is not one this game does.
    pub(crate) fn game_action(&mut self, name: &str) -> bool {
        #[cfg(windows)]
        if self.vr_action(name) { return true; }
        match name {
            "sim_pause" => self.toggle_pause(),
            "screenshot" => self.take_screenshot(),
            "quicksave" => self.quick_save(),
            "view_set_ego" => {
                // on foot from where the camera is (beside the bus in the driver's view)
                if let (Some(cam), Some(p)) = (self.camera.as_mut(), self.player.as_ref()) {
                    if self.view != "free" {
                        let h = (p.vehicle.heading as f32 - 90.0).to_radians();
                        cam.position = p.vehicle.position + glam::DVec3::new(h.sin() as f64, h.cos() as f64, 0.0) * 2.5;
                        cam.yaw = p.vehicle.heading as f32;
                        cam.pitch = 0.0;
                    }
                }
                self.view = "free".into();
                self.ego = true;
                self.service_msg = Some(("On foot: W A S D walk, Shift runs, right mouse button looks (F1 back to the bus)".into(), 5.0));
            }
            "view_set_driver" => self.view = "driver".into(),
            "view_set_passenger" => self.view = "pax".into(),
            "view_set_outside" => self.view = "outside".into(),
            // the cabin (the driver's or the passenger's) and the outside, one press apart:
            // what a single button on a controller wants. `view_toggle_viewpoint` is the
            // four-mode cycle, with the map in it, and stays where it is.
            "view_toggle_interior" => {
                if !self.ego {
                    self.view = if self.view == "outside" { "driver".into() } else { "outside".into() };
                }
            }
            "view_set_map" => {
                // OMSI's map view (F4) is a camera flown over the map; the city map of the
                // navigator stays on Shift+M
                if self.view != "free" {
                    if let (Some(cam), Some(p)) = (self.camera.as_mut(), self.player.as_ref()) {
                        let h = (p.vehicle.heading as f32).to_radians();
                        cam.position = p.vehicle.position
                            + glam::DVec3::new(-(h.sin() as f64) * 25.0, -(h.cos() as f64) * 25.0, 30.0);
                        cam.yaw = p.vehicle.heading as f32;
                        cam.pitch = -45.0;
                    }
                }
                self.view = "free".into();
                self.ego = false;
            }
            // the timetable and the ticket desk each have a camera of their own in the bus
            // (`[view_schedule]`, `[view_ticketselling]`): the key switches the driver's view
            // to it and back
            "view_set_schedule" | "view_set_ticketselling" => {
                let schedule = name == "view_set_schedule";
                if schedule {
                    self.timetable = !self.timetable;
                }
                if let Some(p) = self.player.as_mut() {
                    let def = &p.vehicle.ty.def;
                    let cam = if schedule { def.view_schedule } else { def.view_ticketselling };
                    let n = def.cameras_driver.len().max(1);
                    // (the choice counts from the standard camera)
                    if let Some(c) = cam.filter(|c| *c < def.cameras_driver.len()).map(|c| (c + n - def.camera_std % n) % n) {
                        let back = p.cam_before_special.take();
                        if self.view == "driver" && p.cam_choice.0 == c {
                            p.cam_choice.0 = back.unwrap_or(0);
                        } else {
                            p.cam_before_special = Some(p.cam_choice.0);
                            p.cam_choice.0 = c;
                            self.view = "driver".into();
                        }
                        self.sync_view_look();
                    } else if !schedule {
                        self.service_msg = Some(("This bus has no ticket desk camera".into(), 3.0));
                    }
                }
            }
            "view_toggle_informationdisplay" => self.info_bar = !self.info_bar,
            // (Omsi.exe's camera reset, 0x7edde4, puts back the field of view with the
            // direction: the zoom goes as well, #244)
            "view_reset_direction" => {
                self.look = (0.0, 0.0);
                self.view_zoom.remove(&self.view);
                #[cfg(windows)]
                if let Some(vr) = self.vr.as_mut() { vr.recenter(); }
            }
            // (Space in Inputs/keyboard.cfg: every view looks ahead again, and back to the
            // standard camera - "center")
            "view_reset_all_directions" => {
                self.look = (0.0, 0.0);
                self.view_looks.clear();
                self.view_zoom.clear();
                self.orbit = ORBIT_DEFAULT;
                if let Some(p) = self.player.as_mut() {
                    p.cam_choice = (0, 0);
                }
            }
            // the next (or the previous) view mode, driver - passenger - outside - map and
            // round again; nothing on foot (Omsi.exe 0x706278 @0x70634a: (mode + 1) and 3,
            // @0x706392 the inverse)
            "view_toggle_viewpoint" | "view_toggle_viewpoint_inverse" => {
                if self.ego {
                    return true;
                }
                let mode = match self.view.as_str() {
                    "driver" => 0,
                    "pax" => 1,
                    "outside" => 2,
                    _ => 3,
                };
                let next = if name == "view_toggle_viewpoint" { (mode + 1) % 4 } else { (mode + 3) % 4 };
                return self.game_action(["view_set_driver", "view_set_passenger", "view_set_outside", "view_set_map"][next]);
            }
            // a manual gearbox (Ctrl+Up / Ctrl+Down unless moved; a controller's button)
            "gear_up" | "gear_down" => {
                self.shift_gear(name == "gear_up");
            }
            "view_interiorcam_plus" | "view_interiorcam_minus" => {
                let Some(p) = self.player.as_mut() else { return true };
                // (the interior cameras only cycle in the interior: from outside the keys
                // would change an invisible camera)
                if !matches!(self.view.as_str(), "driver" | "pax") {
                    return true;
                }
                let (count, pax) = if self.view == "pax" { (p.pax_camera_count(), true) } else { (p.driver_camera_count(), false) };
                if count > 1 {
                    let c = if pax { &mut p.cam_choice.1 } else { &mut p.cam_choice.0 };
                    *c = if name == "view_interiorcam_minus" { (*c + count - 1) % count } else { (*c + 1) % count };
                    let n = *c + 1;
                    // (the camera left keeps its look, the one taken finds its own again)
                    self.sync_view_look();
                    self.service_msg = Some((format!("{} camera {n} of {count}", if pax { "Passenger" } else { "Driver" }), 2.0));
                }
            }
            "toggel_mouse_ctrl" => {
                self.set_mouse_drive(!self.mouse_drive);
                let msg = if self.mouse_drive { "Mouse steering on: across steers, up is the throttle, down the brake (O turns it off)" } else { "Mouse steering off" };
                self.service_msg = Some((msg.into(), 4.0));
            }
            "toggel_ctrler" => {
                if let Some(c) = self.controllers.as_mut() {
                    c.enabled = !c.enabled;
                    let msg = if !c.any() { "No game controller found" } else if c.enabled { "Game controller on" } else { "Game controller off" };
                    self.service_msg = Some((msg.into(), 3.0));
                }
            }
            _ => return false,
        }
        true
    }

    /// OMSI's `sim_pause`: the simulation stands still, the camera and the picture go on.
    /// Shift a manual gearbox up or down: the first of the usual trigger names the bus's
    /// scripts have (pressed and let go). False when it has none.
    /// Whether `code` with these modifiers is one of the keyboard file's keys, of the game's
    /// or of the bus's: then a built-in shortcut on it stands back.
    pub(crate) fn chord_bound(&self, code: KeyCode, shift: bool, ctrl: bool, alt: bool) -> bool {
        let m = omsi_content::input::chord(shift, ctrl, alt);
        let Some(scan) = keys::dik_code(code) else { return false };
        self.game_keys.iter().any(|b| b.scan_code == scan && b.matches(m))
            || self.player.as_ref().is_some_and(|p| p.bindings.iter().any(|b| b.scan_code == scan && b.matches(m)))
    }

    pub(crate) fn shift_gear(&mut self, up: bool) -> bool {
        let names: &[&str] = if up {
            &["kw_s_plus", "upshift", "gear_up", "gearup", "shift_up", "gang_hoch", "schalten_hoch", "manual_up"]
        } else {
            &["kw_s_minus", "downshift", "gear_down", "geardown", "shift_down", "gang_runter", "schalten_runter", "manual_down"]
        };
        let Some(p) = self.player.as_mut() else { return false };
        // a gear lever with a trigger per gate (`kw_s_1`..`kw_s_10`, `kw_s_N`, `kw_s_R`: the
        // LiAZ MKPP - its `kw_s_plus` never fires, the script's condition is broken): the
        // next gate from the gear engaged, with the clutch down as the gates want it
        if p.vehicle.ty.program.trigger("kw_s_1").is_some() {
            let Some(cur) = p.gate_gear() else { return false };
            let to = if up { cur + 1 } else { cur - 1 };
            if !p.shift_gate_to(to) {
                return false;
            }
            self.service_msg = Some((format!("Gear {}", match to { 0 => "N".to_string(), -1 => "R".to_string(), n => n.to_string() }), 1.5));
            return true;
        }
        let Some(n) = names.iter().find(|n| p.vehicle.ty.program.trigger(n).is_some()) else {
            self.service_msg = Some(("This vehicle has no manual gearbox to shift".into(), 2.0));
            return false;
        };
        p.vehicle.trigger(n);
        p.vehicle.trigger(&format!("{n}_off"));
        true
    }

    /// Switch the mouse steering on or off, and remember it for the next game. Switched off,
    /// the wheel stays where the mouse left it; switched on, it eases from where it is to
    /// the cursor for the first second.
    pub(crate) fn set_mouse_drive(&mut self, on: bool) {
        self.mouse_drive = on;
        if !on {
            crate::player::keep_wheel(self.player.as_mut());
            // the brake the mouse held stays on, as the brake key leaves it (OMSI has one
            // brake for both): the bus rolled off when the mouse let go of it (#517, #760)
            if let Some(p) = self.player.as_mut() {
                p.axes.brake = p.axes.brake.max(self.mouse_pedals.1);
            }
            #[cfg(windows)]
            self.reset_vr_pointer();
        }
        self.mouse_steer = (self.player.as_ref().map(|p| p.vehicle.physics.controls.steering).unwrap_or(0.0), 1.0);
        self.mouse_pedals = self.player.as_ref().map(|p| (p.vehicle.physics.controls.throttle, p.vehicle.physics.controls.brake)).unwrap_or((0.0, 0.0));
        if self.settings.mouse_steering != on {
            self.settings.mouse_steering = on;
            crate::game_lists::remember_setting("mouse_steering", if on { "1" } else { "0" });
        }
    }

    pub(crate) fn toggle_pause(&mut self) {
        // (a LAN session goes on for the others: it cannot be paused)
        if self.lan.is_some() {
            self.service_msg = Some(("A LAN session cannot be paused".into(), 3.0));
            return;
        }
        self.paused = !self.paused;
        if self.game_menu.is_some() {
            // Keep the state a menu close should restore in step with P.
            self.menu_prev_pause = self.paused;
        }
    }

    /// Put the bus on the street nearest the world point `at` (the city map's Ctrl+click),
    /// facing along it.
    pub(crate) fn place_bus_at(&mut self, at: glam::DVec2) {
        if self.lan.as_ref().is_some_and(|l| l.role == omsi_net::Role::Client) {
            self.service_msg = Some(("In a LAN session only the host moves vehicles on the map".into(), 4.0));
            return;
        }
        let p = glam::DVec3::new(at.x, at.y, 0.0);
        // the traffic's lanes (the tiles loaded around the bus), else the navigator's of the
        // whole map: a street far off on a big map was "no street" until the bus had been
        // flown there (#235). (the height of the point does not matter: the nearest by the
        // ground plan)
        let nets = [self.traffic.as_ref().map(|t| &t.net), self.navigator.as_ref().and_then(|n| n.map_net())];
        let Some((net, (lane, s, _))) = nets
            .into_iter()
            .flatten()
            .find_map(|net| net.nearest_lane(p, omsi_sim::traffic::LaneKind::Street).filter(|(_, _, d)| *d <= 300.0).map(|f| (net, f)))
        else {
            self.service_msg = Some(("No street near that point".into(), 3.0));
            return;
        };
        let l = &net.lanes[lane];
        let (pos, heading) = l.at(s);
        let heading = heading as f64;
        crate::admin::teleport(self, pos, heading);
        self.service_msg = Some(("The bus stands where the map was clicked".into(), 3.0));
    }

    /// The world follows the sim date as OMSI's does at the day's change:
    /// the chrono scenarios in force (the tiles they change are read again) and the
    /// season's textures - also when the weather turns to snow or thaws (every loaded tile
    /// is read again with the other texture folder).
    pub(crate) fn follow_date(&mut self) {
        let Some(w) = self.world.clone() else { return };
        let date = self.clock.date_code();
        let snow = self.weather.as_ref().is_some_and(|x| x.snow);
        let season = crate::world_load::season_folder_on(&self.args, &w.global, self.clock.day_of_year, snow).1;
        let Some((was_date, was_season)) = self.world_day.clone() else {
            self.world_day = Some((date, omsi_texture::season_folder()));
            return;
        };
        if was_date == date && was_season == season {
            return;
        }
        self.world_day = Some((date, season.clone()));
        let changed = if was_date != date { w.set_date(date) } else { Vec::new() };
        let (Some(st), Some(r), Some(scene)) = (self.streamer.as_mut(), self.renderer.as_ref(), self.scene.as_mut()) else { return };
        if was_season != season {
            log::info!("season: the textures of {:?} now (were {:?})", season, was_season);
            omsi_texture::set_season_folder(season);
            omsi_cfg::content_changed();
            st.reload(r, scene, None, self.audio.as_ref());
        } else if !changed.is_empty() {
            st.reload(r, scene, Some(&changed), self.audio.as_ref());
        }
    }

    /// `laststn.osn` in the map's folder (the content folder's copy: the original is never
    /// written), as OMSI keeps it: the launcher offers to continue it. Not
    /// in a tutorial, a LAN session or without a bus of one's own.
    pub(crate) fn save_last_situation(&mut self) -> Option<std::path::PathBuf> {
        if self.tutorial.is_some() || self.lan.is_some() || self.player.is_none() {
            return None;
        }
        let (Some(w), Some(cam)) = (self.world.as_ref(), self.camera.as_ref()) else { return None };
        let dir = std::path::Path::new(&self.args.map.replace('\\', "/")).parent().map(|d| d.to_path_buf())?;
        let base = crate::startup::content_dir()?;
        let dir = base.join(dir);
        let _ = std::fs::create_dir_all(&dir);
        let out = dir.join("laststn.osn");
        let sit = build_situation(&self.args, w, &self.clock, self.args.weather.as_deref(), self.player.as_ref(), &self.placed, cam, self.duty.as_ref(), "Last situation");
        match sit.save(&out) {
            Ok(()) => {
                log::info!("saved the last situation {}", out.display());
                Some(out)
            }
            Err(e) => {
                log::warn!("saving {}: {e}", out.display());
                None
            }
        }
    }

    /// The graphics device was lost (the driver reset the card: it ran out of memory, or a
    /// frame took it too long): the game starts again by itself on the situation just
    /// saved, with lighter graphics (`Settings::apply_safe_gpu`), and on Windows with the
    /// other graphics interface when the lost one was Vulkan. Twice at most in a row. False
    /// when it cannot (a LAN session, the tutorial, nothing to save): the session ends.
    pub(crate) fn restart_after_device_loss(&mut self) -> bool {
        let n = omsi_cfg::env::var("OMSI_SAFE_GPU").ok().and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
        if n >= 2 {
            return false;
        }
        let Some(file) = self.save_last_situation() else { return false };
        let Ok(exe) = std::env::current_exe() else { return false };
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("--root").arg(&self.args.root).arg("--no-menu").arg("--situation").arg(&file);
        // (the duty typed by itself goes on being typed: `--autostart` with a situation)
        if self.player.as_ref().is_some_and(|p| p.duty_typed) {
            cmd.arg("--autostart");
        }
        cmd.env("OMSI_SAFE_GPU", (n + 1).to_string());
        // (on Windows the other interface: DirectX 12 after Vulkan, Vulkan after DirectX 12 -
        // an AMD Radeon's DX12 driver lost the device where its Vulkan one did not, #274)
        let name = self.renderer.as_ref().map(|r| r.adapter_name.clone()).unwrap_or_default();
        if cfg!(windows) {
            if name.contains("(Vulkan)") {
                cmd.env("OMSI_BACKEND", "dx12");
            } else if name.contains("(Dx12)") {
                cmd.env("OMSI_BACKEND", "vulkan");
            }
        }
        match cmd.spawn() {
            Ok(_) => {
                log::warn!("starting again with safer graphics on {} (the graphics device was lost)", file.display());
                true
            }
            Err(e) => {
                log::warn!("could not start the game again: {e}");
                false
            }
        }
    }

    /// Quick save: `Situations/quicksave.osn` next to the game, as OMSI's `quicksave`.
    pub(crate) fn quick_save(&mut self) {
        let (Some(w), Some(cam)) = (self.world.as_ref(), self.camera.as_ref()) else { return };
        // into openOMSI's content folder, never the original installation (the menu and
        // --situation find it there as they find a mod's files)
        let dir = crate::startup::content_dir().unwrap_or_else(|| self.args.root.clone()).join("Situations");
        let _ = std::fs::create_dir_all(&dir);
        let out = dir.join("quicksave.osn");
        let sit = build_situation(&self.args, w, &self.clock, self.args.weather.as_deref(), self.player.as_ref(), &self.placed, cam, self.duty.as_ref(), "Quicksave");
        match sit.save(&out) {
            Ok(()) => {
                log::info!("saved situation {} ({} vehicles)", out.display(), sit.vehicles.len());
                self.service_msg = Some(("Situation saved (quicksave)".into(), 3.0));
            }
            Err(e) => {
                log::warn!("saving {}: {e}", out.display());
                self.service_msg = Some((format!("Could not save: {e}"), 5.0));
            }
        }
    }

    /// A save of its own (#341): the situation into the next free `Saves/Slot <n>.osn` of
    /// the map's folder in the content folder - none is ever overwritten. The launcher
    /// offers them, with the last situation, to continue from.
    pub(crate) fn save_slot(&mut self) {
        let (Some(w), Some(cam)) = (self.world.as_ref(), self.camera.as_ref()) else { return };
        let Some(dir) = crate::startup::content_dir().and_then(|base| {
            std::path::Path::new(&self.args.map.replace('\\', "/")).parent().map(|d| base.join(d).join(SAVES))
        }) else {
            return;
        };
        let _ = std::fs::create_dir_all(&dir);
        let Some(n) = (1..10_000).find(|n| !dir.join(format!("Slot {n}.osn")).exists()) else { return };
        let out = dir.join(format!("Slot {n}.osn"));
        let bus = self.player.as_ref().map(|p| {
            let d = &p.vehicle.ty.def;
            if d.type_name.trim().is_empty() { d.path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default() } else { d.type_name.trim().to_string() }
        });
        let t = self.clock.time;
        let what = match (bus, self.duty.as_ref()) {
            (Some(b), Some(d)) => format!("{b}, line {} / {}", d.line.trim(), d.tour.trim()),
            (Some(b), None) => b,
            (None, _) => "on foot".to_string(),
        };
        let name = format!("Slot {n}: {what}, {:02}:{:02}", (t / 3600.0) as i32 % 24, ((t % 3600.0) / 60.0) as i32);
        let sit = build_situation(&self.args, w, &self.clock, self.args.weather.as_deref(), self.player.as_ref(), &self.placed, cam, self.duty.as_ref(), &name);
        match sit.save(&out) {
            Ok(()) => {
                log::info!("saved situation {} ({} vehicles)", out.display(), sit.vehicles.len());
                self.service_msg = Some((format!("Saved as slot {n}: the launcher continues from it"), 4.0));
            }
            Err(e) => {
                log::warn!("saving {}: {e}", out.display());
                self.service_msg = Some((format!("Could not save: {e}"), 5.0));
            }
        }
    }

    /// OMSI's `screenshot`: the picture into the content folder's `Screenshots`, named by the
    /// date and time.
    pub(crate) fn take_screenshot(&mut self) {
        let dir = crate::startup::content_dir().unwrap_or_else(|| self.args.root.clone()).join("Screenshots");
        let _ = std::fs::create_dir_all(&dir);
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let path = dir.join(format!("omsi_{secs}.png"));
        self.service_msg = Some((format!("Screenshot: {}", path.display()), 4.0));
        self.shot = Some(path);
    }

    /// On foot, the own bus is within reach: inside it, or standing by it (a hand's reach
    /// round its body; a click still has to hit one of its meshes).
    pub(crate) fn foot_reaches_bus(&self) -> bool {
        if self.foot_bus() == Some(crate::humans::BusId::Player) {
            return true;
        }
        match (self.player.as_ref(), self.camera.as_ref()) {
            // (every part of an articulated bus: a door button of the rear section is in
            // reach standing by that section, however far the front one is - #715)
            (Some(p), Some(c)) => {
                let v = &p.vehicle;
                std::iter::once((v.position, v.heading, v.ty.def.bounding_box))
                    .chain(v.trailers.iter().map(|t| (t.position, t.heading, t.ty.def.bounding_box)))
                    .any(|(at, heading, bb)| part_in_reach(c.position, at, heading, bb))
            }
            _ => false,
        }
    }

    pub(crate) fn hud_size(&self) -> (f32, f32) {
        let v = self
            .surface
            .as_ref()
            .map(|s| {
                self.settings
                    .hud_viewport((s.config.width, s.config.height))
            })
            .unwrap_or([0.0, 0.0, 1.0, 1.0]);
        (v[2], v[3])
    }

    pub(crate) fn hud_cursor(&self) -> (f32, f32) {
        let x = self
            .surface
            .as_ref()
            .map(|s| {
                self.settings
                    .hud_viewport((s.config.width, s.config.height))[0]
            })
            .unwrap_or(0.0);
        (self.cursor.0 - x, self.cursor.1)
    }

    pub(crate) fn cockpit_cursor_ray(&self, cam: &Camera, size: (u32, u32)) -> (glam::DVec3, glam::Vec3, f32) {
        #[cfg(windows)]
        if let Some(ray) = self.vr.as_ref().and_then(|vr| vr.cursor_ray(self.cursor.0, self.cursor.1, size)) {
            return (ray.0, ray.1, ray.2 * 6.0);
        }
        if let Some(rig) = self.triple_rig(size) {
            let (o, d, spread) = rig.cursor_ray(cam, self.cursor, size);
            return (o, d, spread * 6.0);
        }
        let (o, d) = cursor_ray(cam, self.cursor.0, self.cursor.1, size.0 as f32, size.1 as f32);
        (o, d, pixel_angle(cam, size.1 as f32) * 6.0)
    }

    /// The triple screen's rig as drawn this frame (the view's zoom applied), while it is on
    /// and no headset is asked for.
    pub(crate) fn triple_rig(&self, size: (u32, u32)) -> Option<omsi_render::TripleScreen> {
        (self.settings.triple.enabled && !self.settings.vr_requested()).then(|| {
            self.settings.triple.zoomed(size.0, size.1, self.view_zoom.get(&self.view).copied().unwrap_or(1.0))
        })
    }

    /// The desktop ray under the cursor for placing and editing on the ground: the window's
    /// own projection, or the triple screen panel's under the cursor.
    pub(crate) fn world_cursor_ray(&self, cam: &Camera, size: (u32, u32)) -> (glam::DVec3, glam::Vec3) {
        match self.triple_rig(size) {
            Some(rig) => {
                let (o, d, _) = rig.cursor_ray(cam, self.cursor, size);
                (o, d)
            }
            None => cursor_ray(cam, self.cursor.0, self.cursor.1, size.0 as f32, size.1 as f32),
        }
    }

    /// With a triple screen, the frustum around all three panels for "nothing appears or
    /// vanishes in sight": tangents of its half-angles, horizontal and vertical (None: the
    /// window's own view is the whole picture).
    pub(crate) fn sight_extent(&self, cam: &Camera, size: (u32, u32)) -> Option<(f64, f64)> {
        let rig = self.triple_rig(size)?;
        let (tx, ty) = rig.view_extent(cam, size.0, size.1);
        Some((tx as f64, ty as f64))
    }

    pub(crate) fn update_hover(&mut self) {
        if self.vr_nav_edit.is_some() || self.cursor_hidden.is_some() {
            self.hover = None;
            self.hover_part = None;
            self.hover_hand = false;
            return;
        }
        #[cfg(windows)]
        if !self.mouse_drive && self.vr.as_ref().is_some_and(|vr| vr.needs_cursor_surface(
            self.cursor, self.game_menu.is_some() || self.chooser.is_some())) {
            let surface = self.player.as_ref()
                .zip(self.camera.as_ref())
                .zip(self.surface.as_ref())
                .filter(|_| matches!(self.view.as_str(), "driver" | "pax"))
                .map(|((player, camera), window)| {
                    let (origin, direction, _) = self.cockpit_cursor_ray(camera,
                                                                         (window.config.width, window.config.height));
                    (player.surface_hit(origin, direction),
                     (player.vehicle.position, player.vehicle.body_rotation()))
                });
            if let Some(vr) = self.vr.as_mut() {
                vr.set_cursor_surface(surface.as_ref().and_then(|s| s.0),
                                      surface.map(|s| s.1));
            }
        }
        let outside = self.view == "outside";
        if let Some(p) = self.player.as_mut() {
            p.occlude_controls = outside;
        }
        let found = match (
            self.player.as_ref(),
            self.camera.as_ref(),
            self.surface.as_ref(),
        ) {
            (Some(p), Some(cam), Some(s)) if self.view != "free"
                && (self.view != "foot" || self.foot_reaches_bus())
                && !(self.vr_active() && self.mouse_drive
                && matches!(self.view.as_str(), "driver" | "pax")) => {
                let (o, d, spread) = self.cockpit_cursor_ray(cam, (s.config.width, s.config.height));
                p.hovered_part(o, d, spread)
            }
            // (in another player's bus nothing is offered: its switches are the driver's)
            _ => (None, false),
        };
        let (found, hand) = found;
        self.hover_hand = hand;
        match found {
            Some((name, true)) => {
                self.hover = Some(name);
                self.hover_part = None;
            }
            Some((name, false)) => {
                self.hover = None;
                self.hover_part = Some(name);
            }
            None => {
                self.hover = None;
                self.hover_part = None;
            }
        }
        // the cursor itself says when it is over something that can be operated
        // (steering with the mouse: a cross, as OMSI shows it; turning the view with the
        // right button held: the four arrows OMSI shows then, #185)
        // (zooming with the mouse: the up-down arrows, Omsi's crSizeNS)
        let kind: u8 = if self.both_drag.is_some() && self.game_menu.is_none() {
            4
        } else if self.mouse_look && self.game_menu.is_none() {
            3
        } else if self.mouse_drive && self.mouse_steers_in_view() && self.game_menu.is_none() {
            2
        } else if self.game_menu.is_some() {
            // (the game menu's own cursor: not overwritten here, or it flips back and forth)
            self.menu_cursor_kind()
        } else if self.hover.is_some() || self.hover_hand {
            1
        } else {
            0
        };
        self.set_cursor_kind(kind);
    }

    /// Show the mouse cursor `kind` (0 arrow, 1 pointing hand, 2 cross, 3 arrows, 4 closed hand).
    pub(crate) fn set_cursor_kind(&mut self, kind: u8) {
        if kind != self.cursor_kind {
            self.cursor_kind = kind;
            if let Some(w) = self.window.as_ref() {
                w.set_cursor(match kind {
                    4 => winit::window::CursorIcon::NsResize,
                    3 => winit::window::CursorIcon::Move,
                    2 => winit::window::CursorIcon::Crosshair,
                    1 => winit::window::CursorIcon::Pointer,
                    _ => winit::window::CursorIcon::Default,
                });
            }
        }
    }

    /// The cursor over the open game menu: a pointing hand over what can be clicked (a line,
    /// a page, a control, the scroll bar), the closed hand while a slider or the scroll bar
    /// is held.
    pub(crate) fn menu_cursor_kind(&self) -> u8 {
        if self.menu_drag.is_some() || self.menu_scroll_drag || self.dd_scroll_drag.is_some() || self.pane_scroll_drag.is_some() {
            return 4;
        }
        let Some(u) = self.ui.as_ref() else { return 0 };
        let (x, y) = self.cursor;
        let inside = |r: &[f32; 4]| x >= r[0] && x <= r[2] && y >= r[1] && y <= r[3];
        let clickable = u.menu_scroll_thumb.is_some_and(|r| inside(&r))
            || u.menu_side.iter().any(|r| inside(r))
            || u.menu_pane.iter().any(|r| inside(r))
            || u.menu_pane_go.as_ref().is_some_and(|r| inside(r))
            || u.menu_time.iter().any(|r| inside(r))
            || u.menu_ctl.iter().flatten().any(|r| inside(r))
            || u.menu_rects.iter().enumerate().any(|(i, r)| inside(r) && !self.menu_item_off(i + u.menu_start));
        if clickable {
            1
        } else {
            0
        }
    }
}

/// Printable fallback for a route number when the window backend supplies no text event.
/// Normal typing uses the actual text event so Shift/layout-specific symbols are kept.
fn route_char(code: KeyCode) -> Option<char> {
    let symbol = match code {
        KeyCode::Minus | KeyCode::NumpadSubtract => Some('-'),
        KeyCode::Equal | KeyCode::NumpadAdd => Some('+'),
        KeyCode::Slash | KeyCode::NumpadDivide => Some('/'),
        KeyCode::NumpadMultiply => Some('*'),
        KeyCode::Period | KeyCode::NumpadDecimal => Some('.'),
        KeyCode::Comma => Some(','),
        KeyCode::Semicolon => Some(';'),
        KeyCode::Quote => Some('\''),
        KeyCode::BracketLeft => Some('['),
        KeyCode::BracketRight => Some(']'),
        KeyCode::Backslash => Some('\\'),
        KeyCode::Backquote => Some('`'),
        _ => None,
    };
    if symbol.is_some() {
        return symbol;
    }
    let name = format!("{code:?}");
    let c = name.strip_prefix("Digit").or_else(|| name.strip_prefix("Numpad")).or_else(|| name.strip_prefix("Key"))?;
    let mut chars = c.chars();
    let ch = chars.next()?;
    (chars.next().is_none() && ch.is_ascii_alphanumeric()).then(|| ch.to_ascii_uppercase())
}

/// The variable a gear lever's gate triggers keep the gear in: `antrieb_getr_aktugang` (the
/// LiAZ, whose gates only move the lever and leave the gear to its frame), else
/// `antrieb_getr_gang` (the stock cars' antrieb.osc), else one named for the gear that both
/// `kw_s_1` and `kw_s_2` set.
pub(crate) fn gate_gear_var(program: &omsi_script::Program) -> Option<String> {
    for known in ["antrieb_getr_aktugang", "antrieb_getr_gang"] {
        if program.var(known).is_some() {
            return Some(known.to_string());
        }
    }
    let mut names: Vec<String> = program
        .var_names()
        .into_iter()
        .filter(|n| n.contains("gang") || n.contains("gear"))
        .filter(|n| {
            let by = program.triggers_setting(n);
            by.iter().any(|t| t == "kw_s_1") && by.iter().any(|t| t == "kw_s_2")
        })
        .collect();
    names.sort_by_key(|n| (n.len(), n.clone()));
    names.into_iter().next()
}

/// Degrees the view turns per (logical) pixel of the cursor's way while looking round:
/// Omsi.exe's fov / 78.75 (TForm_main.Panel1MouseMove 0x82c5f8).
fn look_deg_per_px(fov_deg: f32) -> f32 {
    fov_deg / 78.75
}

/// F3 chase orbit step from raw drag pixels: full turn in yaw at 0.35
/// deg/px (faster than the head's 0.15), pitch between -60 (near top-down)
/// and +25 (just below eye level) around the -15 rest pose, so the camera
/// never swings under the bus. Pure (tested below).
pub(crate) fn chase_orbit_step(yaw: f32, pitch: f32, dx_px: f32, dy_px: f32) -> (f32, f32) {
    const GAIN: f32 = 0.35;
    (
        (yaw + dx_px * GAIN).rem_euclid(360.0),
        (pitch - dy_px * GAIN).clamp(-60.0, 25.0),
    )
}

#[cfg(test)]
mod gear_lever_tests {
    /// The stock cars' gates keep the gear in `antrieb_getr_gang` (#866).
    #[test]
    fn the_gear_is_read_where_the_gates_store_it() {
        let program = |vars: &str, osc: &str| {
            let dir = std::env::temp_dir().join(format!("omsi_gates_{}_{}", std::process::id(), vars.len()));
            std::fs::create_dir_all(&dir).unwrap();
            let (vl, sc) = (dir.join("varlist.txt"), dir.join("antrieb.osc"));
            std::fs::write(&vl, vars).unwrap();
            std::fs::write(&sc, osc).unwrap();
            let p = omsi_script::compile(&omsi_script::CompileInput { varlists: vec![vl], scripts: vec![sc], ..Default::default() });
            let _ = std::fs::remove_dir_all(&dir);
            p
        };
        let stock = program("antrieb_getr_gang\n", "{trigger:kw_s_1_fest}\n{trigger:kw_s_1}\n1 (S.L.antrieb_getr_gang)\n{end}\n{end}\n{trigger:kw_s_2}\n2 (S.L.antrieb_getr_gang)\n{end}\n");
        assert_eq!(super::gate_gear_var(&stock).as_deref(), Some("antrieb_getr_gang"));
        // a lever moved by every gate is not the gear
        let own = program("lever_moved\nmy_gear\n", "{trigger:kw_s_1}\n1 (S.L.lever_moved)\n1 (S.L.my_gear)\n{end}\n{trigger:kw_s_2}\n1 (S.L.lever_moved)\n2 (S.L.my_gear)\n{end}\n");
        assert_eq!(super::gate_gear_var(&own).as_deref(), Some("my_gear"));
    }
}

#[cfg(test)]
mod look_tests {
    /// F1 given to a door and the driver's view moved to 1 (#701): F1 is not the view any more.
    #[test]
    fn a_built_in_view_key_steps_aside_for_the_players_own() {
        use omsi_content::KeyBinding;
        let kb = |a: &str, k: i32| KeyBinding { action: a.into(), scan_code: k, modifier: 0 };
        let none = std::collections::HashSet::new();
        assert!(super::key_left_free(Some(59), "view_set_driver", &none, &[]));
        let own: std::collections::HashSet<i32> = [59].into();
        assert!(!super::key_left_free(Some(59), "view_set_driver", &own, &[]));
        assert!(!super::key_left_free(Some(59), "view_set_driver", &none, &[kb("view_set_driver", 2)]));
        assert!(super::key_left_free(Some(59), "view_set_driver", &none, &[kb("view_set_passenger", 60)]));
    }

    #[test]
    fn a_route_number_takes_digits_letters_and_symbols() {
        use winit::keyboard::KeyCode;
        assert_eq!(super::route_char(KeyCode::Digit5), Some('5'));
        assert_eq!(super::route_char(KeyCode::Numpad0), Some('0'));
        assert_eq!(super::route_char(KeyCode::KeyE), Some('E'));
        assert_eq!(super::route_char(KeyCode::Minus), Some('-'));
        assert_eq!(super::route_char(KeyCode::NumpadSubtract), Some('-'));
        assert_eq!(super::route_char(KeyCode::NumpadAdd), Some('+'));
        assert_eq!(super::route_char(KeyCode::Slash), Some('/'));
        assert_eq!(super::route_char(KeyCode::Space), None);
    }

    #[test]
    fn a_cursor_way_of_78_75_px_turns_by_the_field_of_view() {
        assert!((78.75 * super::look_deg_per_px(60.0) - 60.0).abs() < 1e-4);
    }

    #[test]
    fn chase_orbits_at_035_deg_px_with_stops_above_and_below() {
        // 100 px drag down-right: +35 yaw, -35 pitch.
        let (y, p) = super::chase_orbit_step(0.0, 0.0, 100.0, 100.0);
        assert!((y - 35.0).abs() < 1e-4 && (p + 35.0).abs() < 1e-4, "{y} {p}");
        // yaw wraps the full circle.
        assert!((super::chase_orbit_step(350.0, 0.0, 100.0, 0.0).0 - 25.0).abs() < 1e-3);
        // pitch never leaves the stops, whichever way it is dragged.
        assert_eq!(super::chase_orbit_step(0.0, 0.0, 0.0, -1000.0).1, 25.0);
        assert_eq!(super::chase_orbit_step(0.0, 0.0, 0.0, 1000.0).1, -60.0);
    }
}

/// Write a vehicle's `[scripttexture]` images and `[texttexture]` pictures (with the text
/// they show in the log) into `dir`, to check destination displays and the IBIS.
pub(crate) fn dump_display_textures(vehicle: &omsi_sim::VehicleInstance, dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    for (i, st) in vehicle.host.script_textures.iter().enumerate() {
        let _ = image::save_buffer(
            dir.join(format!("scripttex_{i}.png")),
            &st.rgba,
            st.width,
            st.height,
            image::ColorType::Rgba8,
        );
    }
    for (i, tt) in vehicle.text_textures.iter().enumerate() {
        let text = tt.last_text.clone().unwrap_or_default();
        log::info!(
            "text texture {i} ({} in \"{}\"): {text:?}",
            tt.def.variable,
            tt.def.font
        );
        let (w, h) = (tt.def.width.max(1) as u32, tt.def.height.max(1) as u32);
        if tt.atlas.is_some() {
            let _ = image::save_buffer(
                dir.join(format!("texttex_{i}.png")),
                &tt.image(&text),
                w,
                h,
                image::ColorType::Rgba8,
            );
        }
    }
}

/// `OMSI_INPUT="t=3 move 1045,826; t=3.2 press; ..."` → (time, command) pairs.
pub(crate) fn parse_input_script() -> Vec<(f32, String)> {
    let Ok(v) = omsi_cfg::env::var("OMSI_INPUT") else {
        return Vec::new();
    };
    let mut out: Vec<(f32, String)> = v
        .split(';')
        .filter_map(|item| {
            let item = item.trim();
            let rest = item.strip_prefix("t=")?;
            let (t, cmd) = rest.split_once(' ')?;
            Some((t.trim().parse::<f32>().ok()?, cmd.trim().to_string()))
        })
        .collect();
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    out
}

/// The game menu on a server (`--lan-join https://…`): the world's clock and weather are the
/// server's, and the way out leaves the server.
pub(crate) const SERVER_GAME_MENU: [(&str, &str); 8] = [
    ("resume", "Resume"),
    ("options", "Options..."),
    ("camera", "Camera..."),
    ("vehicle", "Vehicle options..."),
    ("world", "World options..."),
    ("map", "City map"),
    ("shot", "Screenshot"),
    ("quit", "Leave the server"),
];

/// Whether the session was started on a server (`--lan-join https://…`).
pub(crate) fn on_server(args: &crate::Args) -> bool {
    args.lan_join.as_deref().map(|t| omsi_net::ws::ws_url(t).is_some()).unwrap_or(false)
}

/// The game menu's lines for a session with these arguments.
pub(crate) fn game_menu_for(args: &crate::Args) -> &'static [(&'static str, &'static str)] {
    if on_server(args) {
        &SERVER_GAME_MENU
    } else {
        &GAME_MENU
    }
}

impl crate::App {
    /// The game menu's lines for this session: back to the own bus while walking about,
    /// the administration for a host and a server's admin. What can be set is behind
    /// "Options", "Vehicle options" and "World options".
    pub(crate) fn game_menu_items(&self) -> Vec<(&'static str, &'static str)> {
        let mut v: Vec<(&'static str, &'static str)> = game_menu_for(&self.args).to_vec();
        let mut at = 1;
        if self.on_foot.is_some() && self.player.is_some() {
            v.insert(at, ("tobus", "Back to my bus"));
            at += 1;
        }
        // without a bus of one's own: no line to drive
        if self.player.is_none() {
            v.retain(|x| x.0 != "duty");
        }
        // ending the route is offered only while there is one
        if self.duty.is_none() {
            v.retain(|x| x.0 != "endduty");
        }
        if self.navigator.is_none() {
            v.retain(|x| x.0 != "map");
        }
        let host = self.lan.as_ref().map(|l| l.role == omsi_net::Role::Host).unwrap_or(false);
        // the server code: a line to copy it, right under "World options" (only in a LAN session or on a server)
        if self.lan.is_some() || on_server(&self.args) {
            if let Some(w) = v.iter().position(|x| x.0 == "world") {
                v.insert(w + 1, ("copycode", "Copy server code"));
                at = at.max(w + 2);
            }
        }
        if host || self.is_admin {
            let before_quit = v.iter().position(|x| x.0 == "quit").unwrap_or(v.len()).max(at);
            v.insert(before_quit, ("admin", "Administration..."));
        }
        v
    }

    /// Put the session's server code on the clipboard.
    pub(crate) fn copy_server_code(&mut self) {
        // (the host's code; a player or a server's join code or address as it was entered)
        let Some(code) = self.lan.as_ref().and_then(|l| l.code()).map(|c| c.encode()).or_else(|| self.args.lan_join.clone()).filter(|c| !c.trim().is_empty()) else {
            self.service_msg = Some(("No server code: not in a LAN session or on a server".into(), 3.0));
            return;
        };
        #[cfg(not(target_os = "android"))]
        {
            thread_local! {
                // (kept alive: on X11 the text is gone when the clipboard is dropped)
                static CLIPBOARD: std::cell::RefCell<Option<arboard::Clipboard>> = const { std::cell::RefCell::new(None) };
            }
            let ok = CLIPBOARD.with(|c| {
                let mut c = c.borrow_mut();
                if c.is_none() {
                    *c = arboard::Clipboard::new().ok();
                }
                c.as_mut().is_some_and(|cb| cb.set_text(code.clone()).is_ok())
            });
            self.service_msg = Some(if ok { ("Server code copied".into(), 3.0) } else { (format!("{}: {code}", omsi_ui::tr("Server code")), 8.0) });
        }
        #[cfg(target_os = "android")]
        {
            self.service_msg = Some((format!("{}: {code}", omsi_ui::tr("Server code")), 8.0));
        }
    }

    /// The ids of the game menu's lines that are greyed out and cannot be chosen now.
    pub(crate) fn menu_disabled_ids(&self) -> &'static [&'static str] {
        &[]
    }

    /// Whether line `k` of the game menu is greyed out.
    pub(crate) fn menu_item_off(&self, k: usize) -> bool {
        self.chooser.is_none() && self.game_menu_items().get(k).is_some_and(|m| self.menu_disabled_ids().contains(&m.0))
    }

    /// The next line up or down from `from` that can be chosen (round the ends; the greyed-out
    /// lines are skipped).
    fn menu_step(&self, from: usize, n: usize, down: bool) -> usize {
        let mut k = from;
        for _ in 0..n {
            k = if down { (k + 1) % n } else { (k + n - 1) % n };
            if !self.menu_item_off(k) {
                return k;
            }
        }
        from
    }
}

/// The folder of a map's save slots, inside the map's folder in the content folder (the
/// launcher reads it as well: `omsi_launcher_lib::saved_situations`).
pub(crate) const SAVES: &str = "Saves";

/// The lines of the game menu: (what, label). What can be set is on the pages behind
/// "Options", "Vehicle options" and "World options" (see `game_lists`).
pub(crate) const GAME_MENU: [(&str, &str); 13] = [
    ("resume", "Resume"),
    ("options", "Options..."),
    // (the driver's view - seat, field of view, head movement - straight from the pause
    // menu: it is what is changed most while driving, #908)
    ("camera", "Camera..."),
    ("vehicle", "Vehicle options..."),
    ("world", "World options..."),
    ("map", "City map"),
    ("duty", "Line and tour..."),
    ("endduty", "End the tour"),
    ("save", "Save the situation"),
    ("saveslot", "Save to a new slot"),
    ("load", "Load the quicksave"),
    ("shot", "Screenshot"),
    ("quit", "End the session"),
];

/// The keys that fly the free camera (and, with no bus of one's own, the view).
pub(crate) fn flies_free_camera(code: KeyCode) -> bool {
    matches!(code, KeyCode::KeyW | KeyCode::KeyA | KeyCode::KeyS | KeyCode::KeyD | KeyCode::KeyQ | KeyCode::KeyE | KeyCode::Space | KeyCode::ShiftLeft | KeyCode::ArrowLeft | KeyCode::ArrowRight | KeyCode::ArrowUp | KeyCode::ArrowDown)
}

/// `App::key_left_free` for a key's scan code.
pub(crate) fn key_left_free(scan: Option<i32>, action: &str, own: &std::collections::HashSet<i32>, game: &[omsi_content::KeyBinding]) -> bool {
    !scan.is_some_and(|s| own.contains(&s)) && !game.iter().any(|b| b.scan_code != 0 && b.action.eq_ignore_ascii_case(action))
}

/// `App::sync_view_look` for where `self` is borrowed in parts.
/// See `App::look_key`.
pub(crate) fn look_key_of(view: &str, cam: Option<(usize, usize)>) -> String {
    match (view, cam) {
        ("driver", Some((d, _))) => format!("driver#{d}"),
        ("pax", Some((_, x))) => format!("pax#{x}"),
        _ => view.to_string(),
    }
}

/// How far the head turns inside the bus: the driver looks over a shoulder (140 degrees
/// each way, the cab's window pillars and the seat behind), a passenger turns round on
/// the spot - capped at 140 too, a quarter of the coach stayed out of sight (#909). The
/// passenger's turn is kept within -180..180 so that letting go of a glance still swings
/// the short way back.
pub(crate) fn cab_look_yaw(view: &str, yaw: f32) -> f32 {
    if view == "pax" {
        (yaw + 180.0).rem_euclid(360.0) - 180.0
    } else {
        yaw.clamp(-140.0, 140.0)
    }
}

pub(crate) fn swap_view_look(look: &mut (f32, f32), looks: &mut std::collections::HashMap<String, (f32, f32)>, look_view: &mut String, view: &str) {
    if look_view != view {
        let old = std::mem::replace(look_view, view.to_string());
        if !old.is_empty() {
            looks.insert(old, *look);
        }
        *look = looks.get(view).copied().unwrap_or((0.0, 0.0));
    }
}

/// A part of a vehicle (its origin, heading in degrees and `[boundingbox]`) is in reach of
/// a person standing at `eye`: within 3 m of the box's half length round its centre.
fn part_in_reach(eye: glam::DVec3, at: glam::DVec3, heading: f64, bb: Option<[f32; 6]>) -> bool {
    let bb = bb.unwrap_or([2.5, 12.0, 3.0, 0.0, 0.0, 1.5]);
    let h = heading.to_radians();
    let (fwd, right) = (glam::DVec2::new(h.sin(), h.cos()), glam::DVec2::new(h.cos(), -h.sin()));
    let centre = at + (right * bb[3] as f64 + fwd * bb[4] as f64).extend(bb[5] as f64);
    let reach = (bb[0].max(bb[1]) as f64) * 0.5 + 3.0;
    (eye - centre).length() < reach
}

#[cfg(test)]
mod reach_tests {
    use super::part_in_reach;
    use glam::DVec3;

    /// Standing by the rear section of an articulated bus, 17 m behind the front part's
    /// origin: out of the front part's reach, in the rear one's.
    #[test]
    fn the_rear_section_is_reached_by_its_own_box() {
        let front = [2.5, 11.0, 3.0, 0.0, -3.0, 1.5];
        let rear = [2.5, 7.0, 3.0, 0.0, -3.5, 1.5];
        let eye = DVec3::new(2.0, -17.0, 1.7);
        assert!(!part_in_reach(eye, DVec3::ZERO, 0.0, Some(front)));
        assert!(part_in_reach(eye, DVec3::new(0.0, -12.0, 0.0), 0.0, Some(rear)));
        // (the box's centre turns with the part: heading 180, the rear is ahead)
        assert!(part_in_reach(DVec3::new(-2.0, 17.0, 1.7), DVec3::new(0.0, 12.0, 0.0), 180.0, Some(rear)));
    }
}
#[cfg(test)]
mod cab_look_tests {
    use super::cab_look_yaw;

    /// A passenger turns all the way round (#909); the driver still stops over a shoulder.
    #[test]
    fn a_passenger_looks_all_the_way_round() {
        let mut yaw = 0.0;
        for _ in 0..40 {
            yaw = cab_look_yaw("pax", yaw + 10.0);
        }
        // 400 degrees turned: 40 past straight ahead, the short way
        assert!((yaw - 40.0).abs() < 1e-3, "{yaw}");
        assert!((cab_look_yaw("pax", 170.0 + 20.0) + 170.0).abs() < 1e-3);
        assert_eq!(cab_look_yaw("driver", 200.0), 140.0);
        assert_eq!(cab_look_yaw("driver", -200.0), -140.0);
    }
}

/// The first entry a drop-down (or the tours' timetable) of `n` entries showing `rows`
/// shows with its thumb (`len` high) at the top `thumb_top` in `track`.
pub(crate) fn dropdown_top_at(thumb_top: f32, track: [f32; 4], len: f32, n: usize, rows: usize) -> usize {
    let max = n.saturating_sub(rows);
    let travel = (track[3] - track[1] - len).max(1.0);
    let f = ((thumb_top - track[1]) / travel).clamp(0.0, 1.0);
    ((f * max as f32).round() as usize).min(max)
}

#[cfg(test)]
mod dropdown_tests {
    /// A drop-down's thumb dragged down its track scrolls the list to its end (#794).
    #[test]
    fn a_dropdowns_thumb_dragged_scrolls_it() {
        // 40 entries, 8 shown, a 300 px track with a 60 px thumb
        let track = [0.0, 100.0, 4.0, 400.0];
        assert_eq!(super::dropdown_top_at(100.0, track, 60.0, 40, 8), 0);
        assert_eq!(super::dropdown_top_at(340.0, track, 60.0, 40, 8), 32);
        assert_eq!(super::dropdown_top_at(220.0, track, 60.0, 40, 8), 16);
        // past the ends it stays at them
        assert_eq!(super::dropdown_top_at(-50.0, track, 60.0, 40, 8), 0);
        assert_eq!(super::dropdown_top_at(900.0, track, 60.0, 40, 8), 32);
        // a list that fits never scrolls
        assert_eq!(super::dropdown_top_at(300.0, track, 60.0, 5, 8), 0);
    }
}
