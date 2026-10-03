//! The player: their bus, its controls and cameras.

use super::*;

fn indicator_toggle_action(state: &mut u8, lever: Option<u8>, want: u8) -> &'static str {
    // Scripts can cancel the lever themselves after a turn; prefer their current state.
    if let Some(lever) = lever { *state = lever; }
    if want == 3 {
        *state = if *state == 3 { 0 } else { 3 };
        "blinker_warn_toggle"
    } else if *state == want {
        *state = 0;
        "blinker_off"
    } else {
        *state = want;
        if want == 1 { "blinker_left_set" } else { "blinker_right_set" }
    }
}

/// The indicator lever to put back after a frame of the script (None: leave it): where
/// the settings keep an indicator on (`cancel` off), the script turning it off by itself
/// in its frame - a bus whose indicator cancels after a turn, on a phone's wheel after a
/// second or two (#451) - is undone. The player's own keys and clicks run as triggers
/// between the frames, so they still turn it off.
fn kept_indicator(cancel: bool, before: Option<f32>, after: Option<f32>) -> Option<f32> {
    let (before, after) = (before?.round(), after?.round());
    (!cancel && (before == 1.0 || before == 2.0) && after == 0.0).then_some(before)
}

pub(crate) fn steering_view_yaw(current: f32, steering: f32, dt: f32, enabled: bool, angle: f32, response: f32) -> f32 {
    let target = if enabled { steering.clamp(-1.0, 1.0) * angle.clamp(0.0, 60.0) } else { 0.0 };
    current + (target - current) * (1.0 - (-dt.max(0.0) / response.clamp(0.05, 1.0)).exp())
}

fn is_manual_gate_action(name: &str) -> bool {
    let Some(gate) = name.get(..5).filter(|p| p.eq_ignore_ascii_case("kw_s_")).and_then(|_| name.get(5..)) else {
        return false;
    };
    let gate = gate.strip_suffix("_fest").unwrap_or(gate);
    gate.eq_ignore_ascii_case("r") || gate.eq_ignore_ascii_case("n") || gate.parse::<u32>().is_ok()
}

/// Everything about the spawned player vehicle.
pub(crate) struct Player {
    /// Which vehicle this is, for as long as the session runs (the people riding in one
    /// the player left know it by this, see `Humans::player_bus_swapped`).
    pub(crate) uid: u64,
    pub(crate) vehicle: omsi_sim::VehicleInstance,
    pub(crate) render: scene::VehicleRender,
    /// Renders of the coupled parts, parallel to `vehicle.trailers`.
    pub(crate) trailer_renders: Vec<scene::VehicleRender>,
    pub(crate) axes: omsi_sim::KeyboardAxes,
    /// A game controller's pedals and steering this frame (they win over the keys).
    pub(crate) analog: crate::controllers::Analog,
    /// The interior cameras chosen (OMSI's
    /// `view_interiorcam_minus`/`_plus`): the driver's and the passengers' camera numbers.
    pub(crate) cam_choice: (usize, usize),
    /// (scan code, modifier bits) → action name, from `Inputs/keyboard.cfg` `[vehicles]`.
    pub(crate) bindings: Vec<omsi_content::KeyBinding>,
    pub(crate) sounds: Option<omsi_audio::SoundSet>,
    /// Mesh index currently pressed with the mouse (its `[mouseevent]` gets `_off` on release).
    pub(crate) pressed_mesh: Option<usize>,
    /// Coupled-part mesh currently pressed.  Articulated buses keep the rear controls in
    /// the trailer model, while their mouse events are still handled by the lead vehicle.
    pub(crate) pressed_trailer_mesh: Option<(usize, usize)>,
    /// Set while the camera looks at the bus from outside (F3): a switch behind a wall or a
    /// window of the bus is out of reach there, and is neither named, clicked nor turned.
    pub(crate) occlude_controls: bool,
    /// The press on `pressed_mesh`: whether the script has a trigger for the click itself,
    /// and how far the mouse has been dragged since (px).
    pub(crate) press_info: (bool, f32),
    /// A control worked only by dragging (the NL/NG driver's door: `cp_Fahrertuer_drag`
    /// and no click trigger) that was clicked: it is dragged over to its other end.
    pub(crate) auto_drag: Option<AutoDrag>,
    /// Running auto-start (Shift+U).
    pub(crate) startup: Option<omsi_sim::startup::StartUp>,
    /// When the running auto-start began (one that has gone on for long is given up by the
    /// next Shift+U).
    pub(crate) startup_at: Option<std::time::Instant>,
    /// The ticket key was pressed this frame (sell the requested ticket).
    pub(crate) give_ticket: bool,
    /// OMSI's `change_give` / `change_take` keys: hand the passenger
    /// at the desk all the change owed at once / take back what lies on the change tray.
    pub(crate) give_change: bool,
    /// The triggers a controller button's door action (`door_<n>`, `doors_all`) fired, by
    /// action: let go, their `_off` is fired (see [`Player::door_key`]).
    pub(crate) door_buttons: hashbrown::HashMap<String, Vec<String>>,
    /// Parts at the end of the train coupled by hand (they can be uncoupled; an articulated
    /// bus's own rear section cannot).
    pub(crate) hand_coupled: usize,
    /// Bound to the rails (see `rail_drive`), and where on them once it is placed.
    pub(crate) rail_bound: bool,
    pub(crate) rail: Option<crate::rail_drive::RailDrive>,
    /// The driver camera chosen before the timetable or ticket desk camera took over.
    pub(crate) cam_before_special: Option<usize>,
    /// The actions a key started, by its scan code: its release lets go of the same ones,
    /// whatever modifier is held by then (L pressed, Ctrl held, L let go: the release went
    /// to Ctrl+L and the L action stayed held).
    pub(crate) held_keys: hashbrown::HashMap<i32, Vec<String>>,
    /// Where the driver's head is thrown by the bus's accelerations (vehicle frame, m):
    /// OMSI's `[driverview_moving]`.
    pub(crate) head: Vec3,
    /// Its speed (m/s) and the body's turning rates of the frame before (see `move_head`).
    pub(crate) head_vel: Vec3,
    pub(crate) head_omega: Vec3,
    /// What a head does when nothing moves it: the slow sway of a standing body, kept apart
    /// from `head` so the bus's own head movement is never fed it (Settings -> idle head sway;
    /// read through `head_offset`).
    pub(crate) head_idle: crate::head_idle::HeadIdle,
    /// How far the driver's view is turned into the steering (degrees of yaw; see `move_head`).
    pub(crate) steer_look: f32,
    /// The driver's seat moved (Settings → seat position; bus frame, m).
    pub(crate) seat: Vec3,
    /// The player's turn of each mirror (yaw, pitch degrees; Ctrl+Alt+arrows in the cab).
    pub(crate) mirror_offsets: Vec<[f32; 2]>,
    /// The player's shift of each mirror (bus frame, m: across, along, up; the mirror editor).
    pub(crate) mirror_shifts: Vec<[f32; 3]>,
    /// Degrees added to each mirror camera's field of view (the mirror editor).
    pub(crate) mirror_fovs: Vec<f32>,
    /// A mirror was turned or shifted and is not saved yet.
    pub(crate) mirrors_dirty: bool,
    pub(crate) take_change: bool,
    /// Keys whose `_toggle` this bus does as `_up`/`_down` (see `action`): turned up last.
    pub(crate) toggled_up: hashbrown::HashSet<String>,
    /// H-pattern actions act as momentary gear buttons when this is enabled.
    pub(crate) momentary_gears: bool,
    /// The settings' automated manual (#713): a gear lever's gates are worked by the
    /// engine speed (see [`Player::tick_auto_shift`]).
    pub(crate) auto_shift: bool,
    /// Seconds until the automated manual may shift again; the engine's idle speed as seen.
    pub(crate) auto_shift_wait: f32,
    pub(crate) auto_shift_idle: f32,
    /// L switched the side lights on with the headlights (see
    /// [`Player::headlights_with_side_lights`]).
    pub(crate) side_lights_by_l: bool,
    /// The driver figure at the wheel (see `driver.rs`).
    pub(crate) driver: Option<crate::driver::DriverFigure>,
    /// The duty's trip to type into the IBIS once the auto-start has the electrics on:
    /// (line, terminus, the trip's stops, the stop the bus is at: its index and name).
    pub(crate) ibis_duty: Option<(String, String, Vec<String>, (usize, String))>,
    /// The IBIS being typed, with the line and terminus it is typed for.
    pub(crate) ibis_typist: Option<(omsi_sim::ibis::Typist, String, String, Vec<String>)>,
    /// The duty is typed into the IBIS by itself (after Shift+U or `--autostart`): a new
    /// trip of the duty is typed too.
    pub(crate) duty_typed: bool,
    /// The stop a page asked the duty to go on with (`omsi.setNextStop`), for the game to
    /// hand to the duty.
    pub(crate) html_next_stop: Option<usize>,
    /// The IBIS typing looks for its keys on a worker thread (in the window: the trials
    /// would hold a frame up to a second and a half; an offscreen run waits for them).
    pub(crate) ibis_background: bool,
    /// The outside camera's arm (how far out it is swung right now).
    pub(crate) arm: camera_arm::SpringArm,
    /// What `Z`/`X`/`C` last turned on, so a repeat press of the same key turns it back off
    /// (real OMSI's Z/X/C and Shift+numpad 4/6/5 are toggles, not one-shot "set" buttons):
    /// 0 = nothing, 1 = left, 2 = right, 3 = hazard.
    pub(crate) blinker_key_state: u8,
    /// The settings' "Indicators cancel themselves" (`blinker_cancel`): off, the script's
    /// own cancelling after a turn is undone (#451).
    pub(crate) blinker_cancel: bool,
}

// Putting a bus into service (Shift+U, `--autostart`) is `omsi_sim::startup`: it presses
// whatever the vehicle's scripts use to switch the electrics on and to crank the engine
// (found in the compiled scripts, the keys of `Inputs/keyboard.cfg` first) and watches the
// result, so a mod whose ignition key turns one notch per press of E starts as well as the
// stock buses. What is already done is left alone: pressed again (or after `--autostart`)
// the battery toggle used to switch the electrics off and every display went dead. No gear
// is selected at the end: the bus spawns in N anyway, and a D pressed while the auto-start
// was still running was thrown back to N by it.

/// OMSI's own layout drives the bus with Shift and the numpad (throttle Shift+Num 8, brake
/// Shift+Num 2, steering Shift+Num 4/6). A laptop or a Mac keyboard has no numpad at all, so
/// the arrow keys drive as well; all they do in OMSI is step through the interior cameras.
///
/// W, A, S and D drive as well, because that is what everybody reaches for - but
/// `Inputs/keyboard.cfg` gives W the wipers, S the viewpoint and **D the D of the automatic
/// gearbox**, so those three are reached by holding shift (Shift+D selects D), and the bus
/// can still be put into gear. `--drive-keys arrows` leaves W/A/S/D to OMSI entirely.
/// The driving keys of a control preset (`drive_keys` in the settings):
/// `omsi` - only the original layout of Inputs/keyboard.cfg (Shift + numpad), nothing extra;
/// `simple` - W/S/A/D and Up/Down drive (plain Left/Right keep OMSI's interior camera
/// switch, view_interiorcam_minus/plus, Omsi.exe 0x706278; A/D steer); `wasd` - W/S/A/D only;
/// `arrows` - the arrow keys only (W/S/D keep their OMSI meaning: wipers, viewpoint, gear).
pub(crate) fn fallback_action(code: KeyCode, preset: &str) -> Option<omsi_sim::EngineAction> {
    use omsi_sim::EngineAction as A;
    let preset = preset.to_ascii_lowercase();
    let (wasd, arrows) = match preset.as_str() {
        "omsi" | "original" => (false, false),
        "wasd" => (true, false),
        "arrows" => (false, true),
        _ => (true, true), // "simple" and anything unknown
    };
    Some(match code {
        KeyCode::ArrowUp if arrows => A::Throttle,
        KeyCode::ArrowDown if arrows => A::Brake,
        KeyCode::ArrowLeft if arrows && preset == "arrows" => A::SteeringLeft,
        KeyCode::ArrowRight if arrows && preset == "arrows" => A::SteeringRight,
        KeyCode::KeyW if wasd => A::Throttle,
        KeyCode::KeyS if wasd => A::Brake,
        KeyCode::KeyA if wasd => A::SteeringLeft,
        KeyCode::KeyD if wasd => A::SteeringRight,
        _ => return None,
    })
}

/// Keyboard actions of `Inputs/keyboard.cfg` that no vehicle script handles under that
/// name: the original translates them into the triggers the scripts do define (the
/// indicators are `kw_blinker_*` in every stock bus, while the key file says
/// `blinker_left_set`). Each action lists the trigger names tried in turn.
pub(crate) const ACTION_ALIASES: &[(&str, &[&str])] = &[
    (
        "blinker_left_set",
        &["kw_blinker_links", "blinker_links", "cp_blinker_links"],
    ),
    (
        "blinker_right_set",
        &["kw_blinker_rechts", "blinker_rechts", "cp_blinker_rechts"],
    ),
    (
        "blinker_off",
        &["kw_blinker_aus", "blinker_aus", "cp_blinker_aus"],
    ),
    (
        "blinker_warn_toggle",
        &["kw_blinker_warn", "blinker_warn", "cp_warnblinker_toggle"],
    ),
    (
        "parking_brake_toggle",
        &["parking_brake_mouse", "cp_feststellbremse_toggle"],
    ),
];

/// A door leaf's animation variable: `door_0`, `door_2L`, `Door_3` (and the stock scripts'
/// `doorTarget_0` names the same leaf).
fn door_leaf_of(var: &str) -> Option<String> {
    let v = var.to_ascii_lowercase();
    let rest = v.strip_prefix("doortarget_").or_else(|| v.strip_prefix("door_"))?;
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    let tail = &rest[digits..];
    (digits > 0 && tail.len() <= 1 && tail.chars().all(|c| c.is_ascii_alphabetic())).then(|| format!("door_{rest}"))
}

/// The door leaves a trigger moves: the leaf variables (or their targets) it stores, itself
/// or in its macros; failing that, the ones it reads.
fn trigger_leaves(program: &omsi_script::Program, name: &str) -> Vec<String> {
    fn walk(program: &omsi_script::Program, block: omsi_script::BlockId, seen: &mut hashbrown::HashSet<omsi_script::BlockId>, stored: &mut Vec<String>, read: &mut Vec<String>) {
        if !seen.insert(block) || seen.len() > 64 {
            return;
        }
        let Some(b) = program.blocks.get(block as usize) else { return };
        for op in &b.ops {
            match op {
                omsi_script::Op::Store(id) | omsi_script::Op::Load(id) => {
                    let Some(leaf) = program.var_names.get(*id as usize).and_then(|n| door_leaf_of(n)) else { continue };
                    let list = if matches!(op, omsi_script::Op::Store(_)) { &mut *stored } else { &mut *read };
                    if !list.contains(&leaf) {
                        list.push(leaf);
                    }
                }
                omsi_script::Op::Macro(m) => walk(program, *m, seen, stored, read),
                _ => {}
            }
        }
    }
    let Some(b) = program.trigger(name) else { return Vec::new() };
    let (mut stored, mut read) = (Vec::new(), Vec::new());
    walk(program, b, &mut hashbrown::HashSet::new(), &mut stored, &mut read);
    if stored.is_empty() { read } else { stored }
}

/// The doors of a bus front to back, found from the model: the meshes each door leaf's
/// variable animates, and leaves standing within 1.6 m of each other along the bus are one
/// doorway. Each doorway gets the triggers that move its leaves: the toggles
/// (`bus_doorfront<n>`, `bus_dooraft`, a mod's own), or a mod's open and close pair
/// (`bus_door_0` / `bus_door_0_close`, written `open|close`: the one that fits the leaf's
/// state is fired). None when the model's doors cannot be told apart this way.
fn doorways(ty: &omsi_sim::VehicleType) -> Option<Vec<Vec<String>>> {
    // where each leaf is along the bus (the centre of the meshes it moves)
    let mut at: Vec<(String, f32, u32)> = Vec::new();
    for vm in &ty.meshes {
        let Some(def) = ty.model.meshes.get(vm.def_index) else { continue };
        let Some(leaf) = def.animations.iter().find_map(|a| door_leaf_of(&a.variable)) else { continue };
        if vm.data.positions.is_empty() {
            continue;
        }
        let y = vm.data.positions.iter().map(|p| vm.pivot.transform_point3(*p).y).sum::<f32>() / vm.data.positions.len() as f32;
        match at.iter_mut().find(|e| e.0 == leaf) {
            Some(e) => {
                e.1 += y;
                e.2 += 1;
            }
            None => at.push((leaf, y, 1)),
        }
    }
    if at.is_empty() {
        return None;
    }
    let mut leaves: Vec<(String, f32)> = at.into_iter().map(|(l, y, n)| (l, y / n as f32)).collect();
    leaves.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut ways: Vec<Vec<(String, f32)>> = Vec::new();
    for l in leaves {
        match ways.last_mut() {
            Some(w) if (w.last().unwrap().1 - l.1).abs() < 1.6 => w.push(l),
            _ => ways.push(vec![l]),
        }
    }
    let program = &ty.program;
    // which doorways a trigger reaches (its leaves' doorways; a script's branches all count)
    let way_of = |leaf: &String| ways.iter().position(|w| w.iter().any(|(l, _)| l == leaf));
    let reach = |name: &str| -> Vec<usize> {
        let mut v: Vec<usize> = trigger_leaves(program, name).iter().filter_map(way_of).collect();
        v.sort();
        v.dedup();
        v
    };
    // the stock keys by name (`bus_doorfront<n>`, two leaves of a doorway paired), a pair
    // split when its two triggers share no doorway: the LiAZ's `bus_doorfront0` and `1` are
    // its middle and rear doors, and Shift+1 opened both
    let mut groups: Vec<(Vec<String>, usize)> = Vec::new();
    for g in door_trigger_groups(program) {
        if g == ["bus_dooraft"] {
            continue;
        }
        let parts: Vec<Vec<String>> = match (g.first(), g.get(1)) {
            (Some(a), Some(b)) if !reach(a).is_empty() && !reach(b).is_empty() && !reach(a).iter().any(|w| reach(b).contains(w)) => vec![vec![a.clone()], vec![b.clone()]],
            _ => vec![g.clone()],
        };
        for part in parts {
            let front = part.iter().flat_map(|n| reach(n)).min().unwrap_or(usize::MAX);
            groups.push((part, front));
        }
    }
    // doorways no stock key reaches: a mod's own door triggers, all of whose leaves lie in
    // that doorway (a toggle, or an open and close pair written `open|close`)
    let reached: Vec<usize> = groups.iter().flat_map(|(g, _)| g.iter().flat_map(|n| reach(n))).collect();
    let mut names: Vec<&String> = program.triggers.keys().filter(|n| {
        let n = n.to_ascii_lowercase();
        (n.starts_with("bus_door") || n.starts_with("bus_tuer")) && !n.ends_with("_off") && !n.ends_with("_close") && !n.starts_with("bus_doorfront") && n != "bus_dooraft"
    }).collect();
    names.sort();
    for w in 0..ways.len() {
        if reached.contains(&w) {
            continue;
        }
        let mut group: Vec<String> = Vec::new();
        for n in &names {
            let r = reach(n);
            if r != [w] {
                continue;
            }
            let close = format!("{n}_close");
            group.push(if program.trigger(&close).is_some() { format!("{n}|{close}") } else { (*n).clone() });
        }
        if !group.is_empty() {
            groups.push((group, w));
        }
    }
    groups.sort_by_key(|(_, front)| *front);
    let mut out: Vec<Vec<String>> = groups.into_iter().map(|(g, _)| g).collect();
    // a release switch (the SD202's `bus_dooraft`) comes last
    if program.trigger("bus_dooraft").is_some() {
        out.push(vec!["bus_dooraft".to_string()]);
    }
    (!out.is_empty()).then_some(out)
}

/// The door keys of a vehicle type: its doorways from the model ([`doorways`]), else the
/// trigger names ([`door_trigger_groups`]).
pub(crate) fn door_keys(ty: &omsi_sim::VehicleType) -> Vec<Vec<String>> {
    doorways(ty).unwrap_or_else(|| door_trigger_groups(&ty.program))
}

/// Every door of a bus, `Shift+1` first: the stock scripts name a door `bus_doorfront<n>`
/// (the SD200/SD202/EN92 have two, both leaves of the one front door) and the aft one
/// `bus_dooraft` (which is also the stop brake release); a low-floor mod with three or four
/// independent doors, like the O530 Facelift, just has more `bus_doorfront<n>` triggers.
/// The keyboard names the passenger door, not each of its leaves.  Most stock and
/// add-on buses expose the two leaves as consecutive `bus_doorfront<n>` triggers.
/// A few older scripts use the second trigger as a *close* command instead; do not
/// combine that one with the opener (A3/BR275 are examples).
/// Sorted by `n` with `bus_dooraft` last. Consecutive leaves are grouped into one physical
/// doorway unless the script clearly names the second trigger as a close command.
pub(crate) fn door_trigger_groups(program: &omsi_script::Program) -> Vec<Vec<String>> {
    let mut fronts: Vec<(u32, String)> = program
        .triggers
        .keys()
        .filter_map(|n| {
            n.strip_prefix("bus_doorfront")
                .and_then(|rest| rest.parse::<u32>().ok())
                .map(|i| (i, n.clone()))
        })
        .collect();
    fronts.sort_by_key(|(i, _)| *i);
    let mut out = Vec::new();
    let mut i = 0;
    while i < fronts.len() {
        let mut group = vec![fronts[i].1.clone()];
        if let Some((next, name)) = fronts.get(i + 1) {
            if *next == fronts[i].0 + 1 && !door_trigger_closes(program, name) {
                group.push(name.clone());
                i += 1;
            }
        }
        out.push(group);
        i += 1;
    }
    if program.trigger("bus_dooraft").is_some() {
        out.push(vec!["bus_dooraft".to_string()]);
    }
    out
}

/// A door trigger that is a close command of its own (the A3's and BR275's second door key:
/// `1 (S.L.CCW_Tuerschliessen)`): a short block that itself stores into a variable named for
/// closing. Looked for in the macros as well, any mention of such a variable took a bus's
/// second door leaf for one - mod door scripts read `door_close_time` and the like while
/// opening - and Shift+1 moved one leaf, Shift+2 the other.
pub(crate) fn door_trigger_closes(program: &omsi_script::Program, name: &str) -> bool {
    let Some(b) = program.trigger(name).and_then(|b| program.blocks.get(b as usize)) else {
        return false;
    };
    if b.ops.len() > 6 || b.ops.iter().any(|op| matches!(op, omsi_script::Op::Macro(_))) {
        return false;
    }
    b.ops.iter().any(|op| match op {
        omsi_script::Op::Store(id) => {
            let n = program.var_names.get(*id as usize).map(|n| n.to_ascii_lowercase()).unwrap_or_default();
            ["close", "closing", "schliess", "schließ"].iter().any(|x| n.contains(x))
        }
        _ => false,
    })
}

/// The variable a door trigger toggles to say where the leaf is going (`doorTarget_0` of
/// the stock door scripts: the trigger's macros store it), if it has one.
pub(crate) fn door_trigger_target(program: &omsi_script::Program, name: &str) -> Option<omsi_script::VarId> {
    fn find(program: &omsi_script::Program, block: omsi_script::BlockId, seen: &mut hashbrown::HashSet<omsi_script::BlockId>) -> Option<omsi_script::VarId> {
        if !seen.insert(block) {
            return None;
        }
        let b = program.blocks.get(block as usize)?;
        for op in &b.ops {
            match op {
                omsi_script::Op::Store(id) => {
                    let n = program.var_names.get(*id as usize).map(|n| n.to_ascii_lowercase()).unwrap_or_default();
                    if n.contains("target") || n.contains("soll") {
                        return Some(*id);
                    }
                }
                omsi_script::Op::Macro(m) => {
                    if let Some(v) = find(program, *m, seen) {
                        return Some(v);
                    }
                }
                _ => {}
            }
        }
        None
    }
    program.trigger(name).and_then(|b| find(program, b, &mut hashbrown::HashSet::new()))
}

/// Which triggers of a door key's group to fire so that its leaves end up together: when
/// any is open (by its target), only the open ones (to close them), else all. Toggling
/// every leaf of a group made a closed leaf open while an open one closed.
pub(crate) fn door_group_to_fire(v: &mut omsi_sim::VehicleInstance, group: &[String]) -> Vec<String> {
    let fire = door_group_plan(v, group);
    if fire.len() < 2 {
        return fire;
    }
    // Triggers that undo each other are an open and a close key, not two leaves: Road-hog123's
    // door script (the UK buses' - the London Citybus 400, the Enviro400s) opens both leaves
    // on `bus_doorfront0` and closes both on `bus_doorfront1`, and firing the pair left the
    // doors shut: the passengers queued at the closed door for good. Tried on the scripts
    // first (the vehicle left as it was): when the doors' targets end where they began, only
    // the trigger that moves them now is fired.
    let mut targets: Vec<omsi_script::VarId> = group.iter().filter_map(|n| door_trigger_target(&v.ty.program, n.split('|').next().unwrap_or(n))).collect();
    targets.sort_unstable();
    targets.dedup();
    if targets.is_empty() {
        return fire;
    }
    let at = |vars: &[f32]| -> Vec<bool> { targets.iter().map(|&t| vars.get(t as usize).is_some_and(|x| *x > 0.5)).collect() };
    let base = at(&v.state.vars);
    let names: Vec<&str> = fire.iter().map(|s| s.as_str()).collect();
    if at(&v.trial_triggers(&names)) != base {
        return fire;
    }
    match fire.iter().find(|n| at(&v.trial_triggers(&[n.as_str()])) != base) {
        Some(one) => vec![one.clone()],
        None => fire,
    }
}

/// Whether the leaf door trigger `name` works is open now: what the trigger does to the
/// doors' targets when tried (a target it turns down was up), else the target its script
/// names first. The first one named can be a branch not taken: the SD202's
/// `bus_doorfront1` names the rear doors' target (`doorTarget_23`, for a bus whose rear
/// door the front buttons work) before its own leaf's, so with both front leaves open
/// the second Shift+1 shut only one of them.
pub(crate) fn door_trigger_open(v: &mut omsi_sim::VehicleInstance, name: &str) -> Option<bool> {
    let targets: Vec<usize> = v
        .ty
        .program
        .var_names
        .iter()
        .enumerate()
        .filter(|(_, n)| {
            let n = n.to_ascii_lowercase();
            n.contains("target") || n.contains("soll")
        })
        .map(|(k, _)| k)
        .collect();
    if !targets.is_empty() {
        let tried = v.trial_triggers(&[name]);
        let now = &v.state.vars;
        let changed = targets.iter().find(|&&k| (tried.get(k).copied().unwrap_or(0.0) > 0.5) != (now.get(k).copied().unwrap_or(0.0) > 0.5));
        if let Some(&k) = changed {
            return Some(now.get(k).copied().unwrap_or(0.0) > 0.5);
        }
    }
    door_trigger_target(&v.ty.program, name).and_then(|id| v.state.vars.get(id as usize).copied()).map(|x| x > 0.5)
}

/// Which triggers of a door key's group the doors' targets ask for (see
/// `door_group_to_fire`).
fn door_group_plan(v: &mut omsi_sim::VehicleInstance, group: &[String]) -> Vec<String> {
    // an open and close pair (`open|close`): whichever fits the leaf now
    let group: Vec<String> = group
        .iter()
        .map(|n| match n.split_once('|') {
            Some((open, close)) => {
                let leaf = trigger_leaves(&v.ty.program, open);
                let is_open = leaf.iter().any(|l| v.var(l).is_some_and(|x| x > 0.5));
                if is_open { close.to_string() } else { open.to_string() }
            }
            None => n.clone(),
        })
        .collect();
    let group = &group[..];
    let states: Vec<Option<bool>> = group
        .iter()
        .map(|n| door_trigger_open(v, n))
        .collect();
    if group.len() < 2 || states.iter().any(|s| s.is_none()) {
        return group.to_vec();
    }
    let any_open = states.iter().any(|s| *s == Some(true));
    group.iter().zip(&states).filter(|(_, s)| !any_open || **s == Some(true)).map(|(n, _)| n.clone()).collect()
}

/// `Digit1`..`Digit9` as 1..9 (`Digit0` and the numpad digits are left for whatever
/// `Inputs/keyboard.cfg` already puts on them).
pub(crate) fn digit_of(code: KeyCode) -> Option<usize> {
    Some(match code {
        KeyCode::Digit1 => 1,
        KeyCode::Digit2 => 2,
        KeyCode::Digit3 => 3,
        KeyCode::Digit4 => 4,
        KeyCode::Digit5 => 5,
        KeyCode::Digit6 => 6,
        KeyCode::Digit7 => 7,
        KeyCode::Digit8 => 8,
        KeyCode::Digit9 => 9,
        _ => return None,
    })
}

/// The game's own door actions for a controller button (or a key of `keyboard.cfg`) that
/// work on every bus: `door_<n>` is the n-th door front to back, as Shift+n on the
/// keyboard, and `doors_all` every door at once (#916).
pub(crate) fn door_action(name: &str) -> Option<usize> {
    let n = name.to_ascii_lowercase();
    if n == "doors_all" {
        return Some(0);
    }
    n.strip_prefix("door_").and_then(|d| d.parse::<usize>().ok()).filter(|d| (1..=9).contains(d))
}

impl Player {
    /// Door key `n` (1 = the front door; 0 = all of them): the triggers fired. All the doors
    /// close the ones open when any is (leaving the others as they are) and else open them
    /// all; the door release switch (`bus_dooraft` of the Berlin buses) is not a door then.
    pub(crate) fn door_key(&mut self, n: usize) -> Vec<String> {
        let groups = door_keys(&self.vehicle.ty);
        let fire: Vec<String> = if n == 0 {
            let doors: Vec<&Vec<String>> = groups.iter().filter(|g| !(g.len() == 1 && g[0] == "bus_dooraft")).collect();
            let is_open = |v: &mut omsi_sim::VehicleInstance, g: &Vec<String>| {
                g.iter().any(|t| match t.split_once('|') {
                    Some((open, _)) => trigger_leaves(&v.ty.program, open).iter().any(|l| v.var(l).is_some_and(|x| x > 0.5)),
                    None => door_trigger_open(v, t) == Some(true),
                })
            };
            let open: Vec<bool> = doors.iter().map(|g| is_open(&mut self.vehicle, g)).collect();
            let any_open = open.iter().any(|o| *o);
            let mut fire = Vec::new();
            for (g, o) in doors.iter().zip(&open) {
                if !any_open || *o {
                    fire.extend(door_group_to_fire(&mut self.vehicle, g));
                }
            }
            fire
        } else {
            let Some(group) = groups.get(n - 1) else { return Vec::new() };
            let fire = door_group_to_fire(&mut self.vehicle, group);
            // the automatic rear doors of the stock Berlin buses (SD, NL): the key is their
            // release, and switched off with the doors open it shuts them now rather than
            // when the last request has lapsed ("why can I not close the rear doors at all?")
            if group.len() == 1 && group[0] == "bus_dooraft" {
                let v = &mut self.vehicle;
                let release_on = v.var("bremse_halte_sw").is_some_and(|x| x > 0.5);
                let open = v.var("doorTarget_23").is_some_and(|x| x > 0.5);
                if release_on && open && v.var("doorAftLastOpen").is_some() {
                    v.set_var("haltewunsch", 0.0);
                    v.set_var("doorAftLastOpen", 1000.0);
                }
            }
            fire
        };
        log::info!("door key {}: {}", if n == 0 { "all".to_string() } else { n.to_string() }, fire.join(" + "));
        for name in &fire {
            self.vehicle.trigger(name);
        }
        fire
    }

    /// A door key let go: the `_off` of the triggers it fired (the push buttons of the
    /// automatic doors are held between the two).
    pub(crate) fn door_key_off(&mut self, fired: &[String]) {
        for name in fired {
            let off = format!("{name}_off");
            if self.vehicle.ty.program.trigger(&off).is_some() {
                self.vehicle.trigger(&off);
            }
        }
    }

    pub(crate) fn toggle_indicator(&mut self, want: u8) {
        let lever = if self.vehicle.var("lights_sw_warnblinker").is_some_and(|v| v > 0.5) {
            Some(3)
        } else {
            self.vehicle.var("lights_sw_blinker").map(|v| match v.round() as i32 { 1 => 1, 2 => 2, _ => 0 })
        };
        let action = indicator_toggle_action(&mut self.blinker_key_state, lever, want);
        self.action(action, true);
        self.action(action, false);
    }

    /// Some stock roller-blind scripts keep a ratchet position with `max`.  The original
    /// engine resets that ratchet while the hand is moving; without that small engine-side
    /// detail a blind lowered once is immediately snapped back down on every frame.
    pub(crate) fn repair_roller_blind(&mut self, event: &str) {
        let e = event.to_ascii_lowercase();
        if !e.contains("rollo") {
            return;
        }
        if e.ends_with("retract") {
            self.vehicle.set_var("cp_rollo_rastpos", 0.0);
        } else if e.ends_with("drag") {
            if let Some(pos) = self.vehicle.var("cp_rollo_pos") {
                self.vehicle.set_var("cp_rollo_rastpos", pos);
            }
        }
    }

    /// Fire a keyboard action as a script trigger, falling back to the names the stock
    /// scripts use for it. Returns whether any script block ran.
    pub(crate) fn action(&mut self, name: &str, pressed: bool) -> bool {
        if pressed {
            log::info!("action: {name}");
        }
        if name.eq_ignore_ascii_case("blinker_left_toggle") || name.eq_ignore_ascii_case("blinker_right_toggle") {
            if pressed {
                self.toggle_indicator(if name.eq_ignore_ascii_case("blinker_left_toggle") { 1 } else { 2 });
            }
            return true;
        }
        let suffix = if pressed { "" } else { "_off" };
        let release_gear = !pressed
            && self.momentary_gears
            && self.vehicle.ty.program.manual_gearbox()
            && is_manual_gate_action(name);
        // the ticket key of Inputs/keyboard.cfg (T): sell the ticket the passenger at the
        // desk asked for, on buses whose script has no ticket printer
        if let Some(n) = door_action(name) {
            if pressed {
                let fired = self.door_key(n);
                self.door_buttons.insert(name.to_ascii_lowercase(), fired);
            } else if let Some(fired) = self.door_buttons.remove(&name.to_ascii_lowercase()) {
                self.door_key_off(&fired);
            }
            return true;
        }
        if name.eq_ignore_ascii_case("ticket_give") {
            if pressed {
                self.give_ticket = true;
            }
            return true;
        }
        if name.eq_ignore_ascii_case("change_give") || name.eq_ignore_ascii_case("change_take") {
            if pressed {
                if name.eq_ignore_ascii_case("change_give") {
                    self.give_change = true;
                } else {
                    self.take_change = true;
                }
            }
            // the bus's own script may know the key too (a change machine)
            self.vehicle.trigger(&format!("{name}{suffix}"));
            return true;
        }
        if pressed {
            self.clutch_for_gate(name);
        }
        let headlights = pressed && name.eq_ignore_ascii_case("kw_scheinwerfer_toggle");
        let lamps_before = if headlights { self.outside_lamps_lit() } else { 0 };
        if self.vehicle.trigger(&format!("{name}{suffix}")) {
            if release_gear {
                self.select_neutral();
            }
            self.repair_roller_blind(&format!("{name}{suffix}"));
            if headlights {
                self.headlights_with_side_lights(lamps_before);
            }
            return true;
        }
        // a key whose press reached the script's own trigger releases as Omsi.exe does, with
        // `<name>_off` only: an alias's `_off` (parking_brake_mouse_off) would undo it (#420)
        if !pressed && self.vehicle.ty.program.trigger(name).is_some() {
            if release_gear {
                self.select_neutral();
            }
            return true;
        }
        let Some((_, aliases)) = ACTION_ALIASES
            .iter()
            .find(|(a, _)| a.eq_ignore_ascii_case(name))
        else {
            return false;
        };
        for alias in *aliases {
            if self.vehicle.trigger(&format!("{alias}{suffix}")) {
                if release_gear {
                    self.select_neutral();
                }
                self.repair_roller_blind(&format!("{alias}{suffix}"));
                return true;
            }
        }
        let done = self.toggle_as_steps(name, pressed);
        if release_gear {
            self.select_neutral();
        }
        done
    }

    /// A held gate is released into OMSI's neutral trigger; the usual action release still
    /// runs first so buses with an explicit gate-off script retain their own behavior.
    fn select_neutral(&mut self) {
        for name in ["kw_s_N", "kw_s_N_fest"] {
            if self.vehicle.ty.program.trigger(name).is_some() && self.vehicle.trigger(name) {
                self.vehicle.trigger(&format!("{name}_off"));
                break;
            }
        }
    }

    /// OMSI's automatic clutch for a gear lever whose scripts only take a gear with the
    /// clutch pedal right down (`(L.L.clutch) 1 =`, the LiAZ and PAZ KPP) and do not read
    /// `AutoClutch` themselves: a gate chosen with a key, a phone's gear button or a
    /// controller comes with the clutch pressed, let up again as the key lets it (and held
    /// while the bus stands, see [`Player::tick`]). Without it a player with no clutch
    /// pedal - every phone, with the automatic clutch on - could not put a gear in at all
    /// (#226). Scripts that read `AutoClutch` (the Sprinters' G32) work it themselves.
    pub(crate) fn clutch_for_gate(&mut self, name: &str) {
        let Some(gate) = name.get(..5).filter(|p| p.eq_ignore_ascii_case("kw_s_")).map(|_| &name[5..]) else { return };
        let gate = gate.strip_suffix("_fest").unwrap_or(gate);
        let is_gate = gate.eq_ignore_ascii_case("r") || gate.eq_ignore_ascii_case("n") || gate.parse::<u32>().is_ok();
        let program = &self.vehicle.ty.program;
        // (a manual gearbox only: see `Program::manual_gearbox`)
        if !is_gate || !program.manual_gearbox() || self.vehicle.host.auto_clutch < 0.5 || program.trigger(name).is_none() || program.reads_sys(omsi_script::SysVar::AutoClutch) {
            return;
        }
        self.vehicle.set_var("Clutch", 1.0);
        self.axes.clutch = 1.0;
    }

    /// The I key: all saloon light circuits on, or all off. OMSI binds one key to each
    /// circuit (Inputs/keyboard.cfg: 7 `cp_licht_untenrechts_toggle`, 8
    /// `cp_licht_oberdeck_toggle`, 9 `cp_licht_unterdeck_toggle`) and the buses wire them
    /// differently: the stock MANs light the saloon with all three, the LiAZ 5292 with the
    /// first two and nothing on the third - a shortcut that only threw
    /// `cp_licht_unterdeck` never lit the LiAZ. Every saloon switch the keyboard knows is
    /// pressed, and taken back when it moved the model's `[interiorlight]` lamps the
    /// wrong way (see [`Player::set_saloon_lights`]).
    pub(crate) fn toggle_saloon_lights(&mut self) -> String {
        let on = self.saloon_lamps_lit() == 0;
        let done = self.set_saloon_lights(on);
        log::info!("saloon lights {} ({})", if on { "on" } else { "off" }, done.join(", "));
        if self.vehicle.ty.model.interior_lights.is_empty() {
            "This bus has no saloon lights".into()
        } else if !done.is_empty() {
            format!("Saloon lights {}", if on { "on" } else { "off" })
        } else if on {
            // the switches are thrown, but the lamps stay dark: no current
            "Saloon light switches on - no power (battery off?)".into()
        } else {
            "Saloon lights off".into()
        }
    }

    /// How many of the model's `[interiorlight]` lamps are lit now.
    fn saloon_lamps_lit(&self) -> usize {
        self.vehicle
            .ty
            .model
            .interior_lights
            .iter()
            .filter(|l| self.vehicle.var(&l.variable).unwrap_or(0.0) > 0.5)
            .count()
    }

    /// Switch the saloon lights on or off: every saloon light switch `Inputs/keyboard.cfg`
    /// knows (`cp_…licht…`/`…light…`, not the driver's lamp) is pressed once and taken
    /// back (the variables restored) when the scripts moved the lamps the other way. Returns the
    /// switches that did what was asked.
    pub(crate) fn set_saloon_lights(&mut self, on: bool) -> Vec<String> {
        let mut switches: Vec<String> = self
            .bound_actions()
            .into_iter()
            .filter(|a| {
                let a = a.to_ascii_lowercase();
                (a.contains("licht") || a.contains("light"))
                    && a.starts_with("cp_")
                    && !a.contains("fahrer")
                    && !a.contains("driver")
            })
            .collect();
        for extra in ["interior_light_toggle", "interiorlight_toggle"] {
            if self.vehicle.ty.program.triggers.contains_key(extra) {
                switches.push(extra.to_string());
            }
        }
        let total = self.vehicle.ty.model.interior_lights.len();
        let mut done = Vec::new();
        for sw in switches {
            let before = self.saloon_lamps_lit();
            if (on && before == total) || (!on && before == 0) {
                break;
            }
            let saved = self.vehicle.state.vars.clone();
            if !self.action(&sw, true) {
                continue;
            }
            self.action(&sw, false);
            // the scripts set the lamp variables from the switches in their frame
            self.vehicle.update_scripts_only(0.0);
            let after = self.saloon_lamps_lit();
            if (on && after < before) || (!on && after > before) {
                self.vehicle.state.vars = saved;
            } else if after != before {
                done.push(sw);
            }
        }
        done
    }

    /// How many of the model's outside lamps (`[light_enh]`, `[light_enh_2]`) are lit now.
    fn outside_lamps_lit(&self) -> usize {
        let v = &self.vehicle;
        v.ty.model
            .meshes
            .iter()
            .flat_map(|m| {
                m.light_enh
                    .iter()
                    .map(|l| l.variable.as_str())
                    .chain(m.light_enh_2.iter().map(|l| l.variable.as_str()))
            })
            .filter(|n| !n.is_empty() && v.var(n).unwrap_or(0.0) > 0.5)
            .count()
    }

    /// L (`kw_scheinwerfer_toggle`) on a bus whose side and tail lights have a switch of
    /// their own (Ctrl+L, `kw_standlicht_toggle`): switching the headlights on puts those on
    /// too, and switching them off puts them out again when L put them on. The LiAZ 5292's
    /// L lit the two dipped beams and nothing else - no marker lamps on the roof, no tail
    /// lights - and nobody finds Ctrl+L. A bus whose L already lights everything (the stock
    /// MANs' rotary switch) is left as it is: pressing its side-light key would only take
    /// lamps away, and that press is undone.
    fn headlights_with_side_lights(&mut self, before: usize) {
        if !self.vehicle.ty.program.triggers.contains_key("kw_standlicht_toggle") {
            return;
        }
        self.vehicle.update_scripts_only(0.0);
        let after = self.outside_lamps_lit();
        // a press that did not do what was wanted is taken back by restoring the variables
        // as they were: pressing again is no way back on a rotary switch (the EN92's key
        // goes headlights -> side lights -> off)
        let try_press = |p: &mut Player, better: &dyn Fn(usize) -> bool| -> bool {
            let saved = p.vehicle.state.vars.clone();
            p.vehicle.trigger("kw_standlicht_toggle");
            p.vehicle.trigger("kw_standlicht_toggle_off");
            p.vehicle.update_scripts_only(0.0);
            if better(p.outside_lamps_lit()) {
                true
            } else {
                p.vehicle.state.vars = saved;
                false
            }
        };
        if after > before && !self.side_lights_by_l {
            self.side_lights_by_l = try_press(self, &|n| n > after);
        } else if after < before && after > 0 {
            // (switching the headlights off leaves the side lights on only when they were on
            // before the headlights; the lights stayed on for good when that was not known)
            if try_press(self, &|n| n < after) {
                self.side_lights_by_l = false;
            }
        }
    }

    /// A key OMSI binds to a `<switch>_toggle` on a bus whose switch only turns in steps
    /// (`<switch>_up` / `<switch>_down`): the Procity's light switch has
    /// `kw_scheinwerfer_up`/`_down` and nothing answered L (`kw_scheinwerfer_toggle`), so its
    /// headlights could not be switched on from the keyboard. Such a key turns the switch up
    /// two steps (side lights, then headlights) and, the next time, back down.
    pub(crate) fn toggle_as_steps(&mut self, name: &str, pressed: bool) -> bool {
        let Some(base) = name.strip_suffix("_toggle") else {
            return false;
        };
        let (up, down) = (format!("{base}_up"), format!("{base}_down"));
        let has = |v: &omsi_sim::VehicleInstance, t: &str| v.ty.program.triggers.contains_key(&t.to_ascii_lowercase());
        if !has(&self.vehicle, &up) || !has(&self.vehicle, &down) {
            return false;
        }
        if !pressed {
            return true;
        }
        let key = base.to_ascii_lowercase();
        // up when nothing is lit, down when something is (a switch moved by the mouse, or lit
        // from the start, went on up for ever: the lights could not be switched off again)
        let lit = self.outside_lamps_lit() > 0;
        let going_up = if base.to_ascii_lowercase().contains("schein") || base.to_ascii_lowercase().contains("licht") || base.to_ascii_lowercase().contains("light") { !lit } else { !self.toggled_up.contains(&key) };
        let step = if going_up { &up } else { &down };
        for _ in 0..2 {
            self.vehicle.trigger(step);
            self.vehicle.trigger(&format!("{step}_off"));
        }
        if going_up {
            self.toggled_up.insert(key);
        } else {
            self.toggled_up.remove(&key);
        }
        true
    }

    /// Handle a key: engine actions change the axes, everything else fires script triggers.
    pub(crate) fn key(&mut self, scan: i32, modifiers: i32, pressed: bool) {
        let names: Vec<String> = if pressed {
            let n: Vec<String> = self
                .bindings
                .iter()
                .filter(|b| b.scan_code == scan && b.matches(modifiers))
                .map(|b| b.action.clone())
                .collect();
            self.held_keys.insert(scan, n.clone());
            n
        } else {
            match self.held_keys.remove(&scan) {
                Some(n) => n,
                None => self
                    .bindings
                    .iter()
                    .filter(|b| b.scan_code == scan && b.matches(modifiers))
                    .map(|b| b.action.clone())
                    .collect(),
            }
        };
        for name in names {
            if let Some(a) = omsi_sim::engine_action(&name) {
                self.axes.set(a, pressed);
            } else {
                self.action(&name, pressed);
            }
        }
    }

    /// Start the whole bus by itself (Shift+U): everything a driver does to put it into
    /// service. Returns what to show in the HUD.
    pub(crate) fn start_up(&mut self) -> String {
        if let Some(s) = self.startup.as_ref() {
            // (one going on for long - a bus whose switches never get it there - is given up
            // and begun again, rather than saying the same for ever)
            if self.startup_at.is_none_or(|t| t.elapsed().as_secs_f32() < 20.0) {
                return if s.shutting_down() { "Switching the vehicle off ..." } else { "Putting the vehicle into service ..." }.to_string();
            }
            log::info!("auto-start given up after 20 s: begun again");
        }
        let bound = self.bound_actions();
        let s = omsi_sim::startup::StartUp::new(&self.vehicle, &bound);
        let shutting_down = s.shutting_down();
        self.startup = Some(s);
        self.startup_at = Some(std::time::Instant::now());
        if shutting_down {
            "Switching the vehicle off ...".to_string()
        } else {
            "Putting the vehicle into service ...".to_string()
        }
    }

    /// The vehicle actions `Inputs/keyboard.cfg` puts on keys.
    pub(crate) fn bound_actions(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for b in &self.bindings {
            if !names.iter().any(|n| n.eq_ignore_ascii_case(&b.action)) {
                names.push(b.action.clone());
            }
        }
        names
    }

    /// Advance a running auto-start, then the IBIS typing; true while the auto-start is
    /// still going.
    pub(crate) fn tick_startup(&mut self, dt: f32) -> bool {
        self.apply_html_requests();
        self.tick_ibis(dt);
        let Some(mut s) = self.startup.take() else {
            return false;
        };
        let bound = self.bound_actions();
        if s.tick(&mut self.vehicle, &bound, dt) {
            self.startup = Some(s);
            return true;
        }
        log::info!(
            "auto-start finished: {} (engine_n={:?})",
            if s.report.is_empty() {
                "nothing to do".to_string()
            } else {
                s.report.join(", ")
            },
            self.vehicle.var("engine_n")
        );
        if omsi_sim::startup::power_on(&self.vehicle) {
            self.saloon_lights_after_dark();
        }
        if let Some((line, terminus, stops, stop)) = self.ibis_duty.take() {
            self.type_destination(&line, &terminus, &stops, (stop.0, &stop.1));
        }
        false
    }

    /// After dark a bus put into service (Shift+U) has its saloon lit, as a driver switches it
    /// on before the first stop: every key `Inputs/keyboard.cfg` gives a saloon light switch
    /// is pressed once, and pressed again when that put out more of the model's
    /// `[interiorlight]` lamps than it lit. Before, the saloon stayed dark until the
    /// switches were found (the LiAZ has two, on 7 and 8).
    fn saloon_lights_after_dark(&mut self) {
        let light_out = !omsi_sim::Daylight::compute(&self.vehicle.host.clock, None).lamps_on;
        if light_out {
            return;
        }
        // the headlights as well (the bus was put into service at night with its saloon
        // lit and its headlights off): the light switch key once, where the headlights are
        // off (`Spot_Select` < 0, the stock scripts' "no spotlight")
        let headlights_off = self.vehicle.var("Spot_Select").is_some_and(|s| s < 0.0);
        if headlights_off && self.vehicle.ty.program.trigger("kw_scheinwerfer_toggle").is_some() {
            self.action("kw_scheinwerfer_toggle", true);
            self.action("kw_scheinwerfer_toggle", false);
            log::info!("auto-start: headlights switched on after dark");
        }
        if self.vehicle.ty.model.interior_lights.is_empty() {
            return;
        }
        let done = self.set_saloon_lights(true);
        if !done.is_empty() {
            log::info!("auto-start: saloon lights switched on after dark ({})", done.join(", "));
        }
    }

    /// Type on into the IBIS; when the typing could not be done, the IBIS variables are
    /// written directly.
    pub(crate) fn tick_ibis(&mut self, dt: f32) {
        let Some((mut typist, line, terminus, stops)) = self.ibis_typist.take() else {
            return;
        };
        if typist.tick(&mut self.vehicle, dt) {
            self.ibis_typist = Some((typist, line, terminus, stops));
            return;
        }
        match typist.outcome() {
            Some(Ok(what)) => log::info!("IBIS set to line {line} terminus {terminus}: {what}"),
            other => {
                log::info!(
                    "IBIS: {}; line {line} terminus {terminus} set directly",
                    other
                        .and_then(|o| o.as_ref().err())
                        .cloned()
                        .unwrap_or_default()
                );
                let hof = self.vehicle.host.hof.clone();
                let stops: Vec<&str> = stops.iter().map(String::as_str).collect();
                schedule::set_player_destination_directly(
                    &mut self.vehicle,
                    hof.as_deref(),
                    &line,
                    &terminus,
                    &stops,
                );
            }
        }
    }

    /// A page moved the duty to stop `stop` of `trip` (`omsi.setNextStop`): the IBIS follows,
    /// forwards or backwards, by `IBIS_busstop` (its keys only count up, so the typist cannot
    /// do this). Without a route in the IBIS there is nothing to move.
    pub(crate) fn ibis_to_stop(&mut self, trip: &schedule::PlannedTrip, stop: usize) {
        let Some(hof) = self.vehicle.host.hof.clone() else { return };
        let Some(route) = self.vehicle.var("IBIS_RouteIndex").filter(|r| *r >= 0.0) else { return };
        if self.vehicle.var("IBIS_busstop").is_none() {
            return;
        }
        let Some(name) = trip.stops.get(stop).map(|s| s.name.clone()) else { return };
        if let Some(i) = schedule::ibis_stop_index(&hof, route.round() as usize, &name, stop) {
            self.vehicle.set_var("IBIS_busstop", i as f32);
        }
    }

    /// Put the duty's trip on the IBIS, at the stop the bus is at: now, or when the running
    /// auto-start is done.
    pub(crate) fn set_duty_destination(&mut self, trip: &schedule::PlannedTrip, stop: usize) {
        self.duty_typed = true;
        let stops: Vec<String> = trip.stops.iter().map(|s| s.name.clone()).collect();
        let name = trip
            .stops
            .get(stop)
            .map(|s| s.name.clone())
            .unwrap_or_default();
        if self.startup.is_some() {
            self.ibis_duty = Some((
                trip.line.clone(),
                trip.terminus.clone(),
                stops,
                (stop, name),
            ));
        } else {
            self.type_destination(&trip.line, &trip.terminus, &stops, (stop, &name));
        }
    }

    pub(crate) fn type_destination(
        &mut self,
        line: &str,
        terminus: &str,
        stops: &[String],
        stop: (usize, &str),
    ) {
        if let Some((mut old, ..)) = self.ibis_typist.take() {
            old.abandon(&mut self.vehicle);
        }
        let hof = self.vehicle.host.hof.clone();
        // the keys a driver reaches: the cockpit's clickable switches and the keyboard's
        let mut keys: hashbrown::HashSet<String> = self
            .vehicle
            .ty
            .model
            .meshes
            .iter()
            .filter_map(|m| m.mouse_event.as_ref())
            .map(|e| e.trim().to_ascii_lowercase())
            .collect();
        keys.extend(
            self.bound_actions()
                .iter()
                .map(|a| a.trim().to_ascii_lowercase()),
        );
        let operable = |name: &str| keys.contains(&name.trim().to_ascii_lowercase());
        let names: Vec<&str> = stops.iter().map(String::as_str).collect();
        match schedule::player_ibis(
            &mut self.vehicle,
            hof.as_deref(),
            line,
            terminus,
            &names,
            Some(stop),
            &operable,
            self.ibis_background,
        ) {
            Some(typist) => {
                self.ibis_typist = Some((
                    typist,
                    line.to_string(),
                    terminus.to_string(),
                    stops.to_vec(),
                ))
            }
            None => log::info!("IBIS: nothing to type for line {line} terminus {terminus}"),
        }
    }

    /// What the bus's HTML pages asked of the IBIS (`omsi.setRoute`, `omsi.setLine`,
    /// `omsi.setDestination`, `omsi.setNextStop`): a route is typed as a driver does (line, route, destination),
    /// a destination alone is written to the sign.
    pub(crate) fn apply_html_requests(&mut self) {
        let requests = self.vehicle.take_html_requests();
        if requests.is_empty() {
            return;
        }
        let (stops, requests): (Vec<_>, Vec<_>) = requests.into_iter().partition(|r| matches!(r, omsi_sim::htmltex::HtmlRequest::SetNextStop(_)));
        if let Some(omsi_sim::htmltex::HtmlRequest::SetNextStop(i)) = stops.last() {
            log::info!("HTML page: setNextStop({i}) received");
            self.html_next_stop = Some(*i);
        }
        if requests.is_empty() {
            return;
        }
        let Some(hof) = self.vehicle.host.hof.clone() else {
            log::info!("HTML page: the bus has no depot file, {requests:?} ignored");
            return;
        };
        for req in requests {
            match req {
                omsi_sim::htmltex::HtmlRequest::SetRoute(i) => self.set_route_from_page(&hof, i),
                omsi_sim::htmltex::HtmlRequest::SetLine(line) => {
                    let wanted = line.trim();
                    let digits: String = wanted.chars().take_while(|c| c.is_ascii_digit()).collect();
                    let found = hof
                        .info_trips
                        .iter()
                        .position(|t| route_line(t).eq_ignore_ascii_case(wanted))
                        .or_else(|| hof.info_trips.iter().position(|t| !digits.is_empty() && route_line(t) == digits));
                    match found {
                        Some(i) => self.set_route_from_page(&hof, i),
                        None => log::info!("HTML page: depot file {} has no line '{wanted}'", hof.name),
                    }
                }
                omsi_sim::htmltex::HtmlRequest::SetNextStop(_) => {}
                omsi_sim::htmltex::HtmlRequest::ClearLine => {
                    if let Some((mut old, ..)) = self.ibis_typist.take() {
                        old.abandon(&mut self.vehicle);
                    }
                    for n in ["IBIS_LinieKurs", "IBIS_Linie_Complex", "IBIS_Linie_Suffix"] {
                        self.vehicle.set_var(n, 0.0);
                    }
                    self.vehicle.set_var("IBIS_RouteIndex", -1.0);
                    if let Some(i) = self.vehicle.ty.program.str_var("IBIS_Complex_Line") {
                        self.vehicle.state.str_vars[i as usize] = "     ".into();
                    }
                    if let Some(i) = self.vehicle.ty.program.str_var("SetLineTo") {
                        self.vehicle.state.str_vars[i as usize] = String::new();
                    }
                    log::info!("HTML page: line cleared");
                }
                omsi_sim::htmltex::HtmlRequest::SetDestination(ti) => {
                    let Some(term) = hof.termini.get(ti) else {
                        log::info!("HTML page: depot file {} has no destination {ti}", hof.name);
                        continue;
                    };
                    let line = {
                        let l = self.vehicle.host.tt_line.trim().to_string();
                        if !l.is_empty() {
                            l
                        } else {
                            let c = self.vehicle.str_var("IBIS_Complex_Line").trim().to_string();
                            if !c.is_empty() {
                                c
                            } else {
                                self.vehicle.var("IBIS_LinieKurs").filter(|n| *n > 0.5).map(|n| format!("{}", n.round() as i64)).unwrap_or_default()
                            }
                        }
                    };
                    let wanted = if term.texture_id.trim().is_empty() { term.strings.first().cloned().unwrap_or_default() } else { term.texture_id.clone() };
                    log::info!("HTML page: destination {ti} '{wanted}' on line '{line}'");
                    if let Some((mut old, ..)) = self.ibis_typist.take() {
                        old.abandon(&mut self.vehicle);
                    }
                    schedule::set_player_destination_directly(&mut self.vehicle, Some(&hof), &line, &wanted, &[]);
                }
            }
        }
    }

    /// Type route `i` of the depot file (`omsi.depot.routes[i]`) into the IBIS.
    fn set_route_from_page(&mut self, hof: &omsi_vehicle::hof::Hof, i: usize) {
        let Some(t) = hof.info_trips.get(i) else {
            log::info!("HTML page: depot file {} has no route {i}", hof.name);
            return;
        };
        let code = omsi_cfg::parse_i32(&t.route);
        let Some(term) = hof.termini.iter().find(|x| x.code == code) else {
            log::info!("HTML page: route {i} ({}) leads to destination code {code}, which the depot file lacks", t.name.trim());
            return;
        };
        // (the route's own stops, so that this route is typed and not another of the line to
        // the same terminus)
        let stops: Vec<String> = hof.info_busstop_lists.get(i).map(|l| l.iter().map(|s| s.trim().to_string()).collect()).unwrap_or_default();
        let name = stops.first().cloned().unwrap_or_default();
        let wanted = if term.texture_id.trim().is_empty() { term.strings.first().cloned().unwrap_or_default() } else { term.texture_id.clone() };
        let line = route_line(t);
        log::info!("HTML page: route {i} '{}' (line '{line}' (file: '{}') to '{wanted}')", t.name.trim(), t.line.trim());
        self.type_destination(&line, &wanted, &stops, (0, &name));
    }

    /// The driver's head follows the bus's accelerations a little late, as a body does:
    /// forward when braking, out of a bend, down over a bump (OMSI's head movement).
    /// The driver's head on its neck, as Omsi.exe moves the driver's view (0x7e2110): a mass
    /// on a spring, thrown by what the body does where the eye is - up and down always
    /// (spring 3000/150, damper 2000/150 per second, a kick of the heave's acceleration and
    /// of the pitch and roll rates' change times the eye's lever), sideways and fore and aft
    /// with `[driverview_moving]` (3000/100 and 2000/100, and only once the bus is moving),
    /// never further than 10 cm up or down (0x7e2256). (A lag of our own towards a point a
    /// hundredth of the acceleration off - a third of what the original throws the head -
    /// stood in for it.)
    pub(crate) fn move_head(&mut self, dt: f32, enabled: bool) {
        let dt = dt.clamp(0.0, 0.1);
        let a = self.vehicle.physics.accel;
        let omega = self.vehicle.rigid.as_ref().map(|rb| rb.omega).unwrap_or(Vec3::ZERO);
        let dw = omega - self.head_omega;
        self.head_omega = omega;
        if dt <= 0.0 {
            return;
        }
        let def = &self.vehicle.ty.def;
        let n = def.cameras_driver.len().max(1);
        let eye = def.cameras_driver.get((def.camera_std + self.cam_choice.0) % n).map(|c| Vec3::new(c.pos[0], c.pos[1], c.pos[2])).unwrap_or(Vec3::ZERO);
        // (1 unless a frame is longer than 1/15 s)
        let stab = (1.0 / (15.0 * dt)).min(1.0);
        let spring = |p: f32, v: f32, kick: f32, k: f32, c: f32| -> (f32, f32) {
            let v = v + kick + (-k * p - c * v) * stab * dt;
            (p + v * dt, v)
        };
        // up and down: the heave's acceleration, and the roll and pitch rates' change at the eye
        let kick = -(a.z - 9.81) * dt + dw.y * eye.x + dw.x * eye.y;
        let (mut p, mut v) = spring(self.head.z, self.head_vel.z, kick, 3000.0 / 150.0, 2000.0 / 150.0);
        if p.abs() > 0.1 {
            p = p.clamp(-0.1, 0.1);
            v = 0.0;
        }
        self.head.z = p;
        self.head_vel.z = v;
        if enabled {
            let moving = self.vehicle.physics.speed.abs().min(1.0);
            let kx = (-a.x * dt + dw.y * eye.z - dw.z * eye.y) * moving;
            let ky = (-a.y * dt - dw.x * eye.z + dw.z * eye.x) * moving;
            let (px, vx) = spring(self.head.x, self.head_vel.x, kx, 3000.0 / 100.0, 2000.0 / 100.0);
            let (py, vy) = spring(self.head.y, self.head_vel.y, ky, 3000.0 / 100.0, 2000.0 / 100.0);
            self.head.x = px;
            self.head_vel.x = vx;
            self.head.y = py;
            self.head_vel.y = vy;
        } else {
            self.head.x = 0.0;
            self.head.y = 0.0;
            self.head_vel.x = 0.0;
            self.head_vel.y = 0.0;
        }
    }

    /// The head of someone who is doing nothing at all: a standing body breathes and shifts
    /// its weight, so the view is never quite still while the bus waits at a stop (Settings ->
    /// idle head sway, off by default). `strength` is how much of the sway is asked for and
    /// `pace` how fast it is to move, against the pace it is designed at.
    /// Nothing of it is written into `head`: the springs above stay the bus's business alone.
    pub(crate) fn move_head_idle(&mut self, dt: f32, strength: f32, pace: f32) {
        self.head_idle.step(dt, strength, pace);
    }

    /// Where the driver's eye is in the bus's frame: the head OMSI's head movement throws
    /// about, with the sway of a head at rest on top of it (`head` and `head_idle`, added
    /// nowhere else - `driver_world`, `camera_look` and the mirrors' `driver_eye` all ask
    /// here, so they cannot disagree).
    pub(crate) fn head_offset(&self) -> Vec3 {
        self.head + self.head_idle.offset
    }

    /// The automatic clutch of the settings for a gear lever whose scripts do not read
    /// OMSI's `AutoClutch` (the LiAZ MKPP): with a gear in and the bus slow, the clutch
    /// bites as the throttle goes down, as a driver lets it up - without it every start
    /// from a stop stalled the engine unless a clutch pedal was worked.
    pub(crate) fn auto_clutch_bite(&mut self, throttle: f32) {
        let program = &self.vehicle.ty.program;
        let has_manual_gate = program.manual_gearbox();
        // (only a gearbox that reads the clutch pedal: an automatic whose scripts answer to
        // the number keys as well had its clutch pressed at every stop, and some went to
        // neutral when it stood, #234)
        let reads_clutch = program.var("Clutch").is_some_and(|v| program.reads(v));
        if self.vehicle.host.auto_clutch < 0.5 || !has_manual_gate || !reads_clutch || program.reads_sys(omsi_script::SysVar::AutoClutch) {
            return;
        }
        let gear = self.vehicle.var("antrieb_getr_aktugang").or_else(|| self.vehicle.var("antrieb_getr_gang")).unwrap_or(0.0);
        let kmh = self.vehicle.physics.velocity_kmh().abs();
        if gear.abs() > 0.5 && kmh < 12.0 {
            // it bites as the throttle goes down and only as far as the engine keeps its
            // revs (a clutch let go at once under full throttle stalled it all the same)
            // (a script that keeps its revs under another name: by the throttle alone - taken
            // as 0 revs the clutch never bit and the bus stood with it down, #260)
            let n = self.vehicle.var("engine_n").unwrap_or(2000.0);
            let bite = ((throttle - 0.05) / 0.45).clamp(0.0, 1.0).min(((n - 850.0) / 700.0).clamp(0.0, 1.0));
            let bite = bite * bite * (3.0 - 2.0 * bite);
            let want = (1.0 - bite) * (1.0 - kmh / 12.0);
            self.axes.clutch = self.axes.clutch.max(want);
        }
    }

    /// The gear a gear lever's gates have engaged (`kw_s_1`.. buses and cars): the variable
    /// the gates store (`antrieb_getr_gang` in the stock cars, `antrieb_getr_aktugang` in the
    /// LiAZ). None without gates.
    pub(crate) fn gate_gear(&self) -> Option<i32> {
        let program = &self.vehicle.ty.program;
        program.trigger("kw_s_1")?;
        let v = crate::input_script::gate_gear_var(program).and_then(|v| self.vehicle.var(&v));
        Some(v.unwrap_or(0.0).round() as i32)
    }

    /// Put a gear lever into gate `to` (-1 R, 0 N): as a driver does it, the clutch down, the
    /// gear in, the clutch let up over a second and a half as OMSI's clutch key lets it (let
    /// go at once, a bus pulling away stalled its engine). False when there is no such gate.
    pub(crate) fn shift_gate_to(&mut self, to: i32) -> bool {
        let name = match to {
            0 => "kw_s_N".to_string(),
            -1 => "kw_s_R".to_string(),
            n => format!("kw_s_{n}"),
        };
        if to < -1 || self.vehicle.ty.program.trigger(&name).is_none() {
            return false;
        }
        self.vehicle.set_var("Clutch", 1.0);
        self.axes.clutch = 1.0;
        self.vehicle.trigger(&name);
        self.vehicle.trigger(&format!("{name}_off"));
        true
    }

    /// The automated manual (the settings' `auto_shift`, #713; not an OMSI feature): on a
    /// manual gearbox worked through gates, first gear goes in when the throttle is pressed
    /// in neutral at a standstill, the next gear up once the engine turns well above its idle
    /// (later the more throttle), and the next down when it falls back towards it. The
    /// clutch is worked as for a shift by key.
    pub(crate) fn tick_auto_shift(&mut self, dt: f32, throttle: f32, brake: f32) {
        self.auto_shift_wait = (self.auto_shift_wait - dt).max(0.0);
        if !self.auto_shift || !self.vehicle.ty.program.manual_gearbox() {
            return;
        }
        let Some(cur) = self.gate_gear() else { return };
        let Some(n) = ["engine_n", "antrieb_eng_n", "engine_rpm", "motor_n", "motor_rpm"].iter().find_map(|v| self.vehicle.var(v)) else { return };
        let kmh = self.vehicle.physics.velocity_kmh().abs();
        // the idle speed, from the engine running free at a standstill
        if n > 300.0 && kmh < 1.0 && throttle < 0.02 && (cur == 0 || self.axes.clutch > 0.9) {
            self.auto_shift_idle = if self.auto_shift_idle > 0.0 { self.auto_shift_idle + (n - self.auto_shift_idle) * (dt / 2.0).min(1.0) } else { n };
        }
        // (not while the clutch is still coming up from the last shift - except in neutral,
        // or rolling to a stop, where the automatic clutch holds it down anyway)
        if n < 300.0 || self.auto_shift_wait > 0.0 || (self.axes.clutch > 0.5 && cur != 0 && kmh >= 5.0) {
            return;
        }
        let idle = if self.auto_shift_idle > 300.0 { self.auto_shift_idle } else { 700.0 };
        let top = (1..=12).take_while(|g| self.vehicle.ty.program.trigger(&format!("kw_s_{g}")).is_some()).last().unwrap_or(0);
        let to = auto_shift_gear(cur, top, n / idle, throttle, brake, kmh);
        if to != cur && self.shift_gate_to(to) {
            self.auto_shift_wait = 1.5;
        }
    }

    pub(crate) fn tick(&mut self, dt: f32, audio: Option<&omsi_audio::AudioEngine>, inside: bool, listener_follows_bus: bool) {
        self.tick_startup(dt);
        self.tick_auto_drag(dt);
        self.axes.speed_kmh = self.vehicle.physics.velocity_kmh();
        self.axes.lock_curvature = self.vehicle.ty.def.inv_min_turn_radius;
        self.axes.update(dt);
        let a = self.analog;
        // a throttle pedal takes off a brake the keys hold (`pedal_hold`), as the throttle
        // key does: a brake tapped on the keys or a wheel's button stayed on under the
        // pedal and the bus was driven against its brakes (#377)
        if a.throttle.is_some_and(|t| t > 0.05) {
            self.axes.brake = 0.0;
        }
        self.auto_clutch_bite(a.throttle.unwrap_or(0.0).max(self.axes.throttle));
        self.tick_auto_shift(dt, a.throttle.unwrap_or(0.0).max(self.axes.throttle), a.brake.unwrap_or(0.0).max(self.axes.brake));
        self.vehicle.set_controls(omsi_sim::Controls {
            throttle: a.throttle.unwrap_or(self.axes.throttle).max(self.axes.throttle),
            brake: a.brake.unwrap_or(self.axes.brake).max(self.axes.brake),
            clutch: a.clutch.unwrap_or(self.axes.clutch).max(self.axes.clutch),
            // (a wheel at rest does not hold against the keys)
            steering: match a.steering {
                Some(s) if s.abs() > 0.02 || self.axes.steering == 0.0 => s,
                _ => self.axes.steering,
            },
        });
        let lever = self.vehicle.var("lights_sw_blinker");
        self.vehicle.update(dt);
        if let Some(keep) = kept_indicator(self.blinker_cancel, lever, self.vehicle.var("lights_sw_blinker")) {
            self.vehicle.set_var("lights_sw_blinker", keep);
        }
        // OMSI_SUSP_TRACE_WINDOW=<csv>: each wheel's travel every frame of a window run
        // (the offscreen run has OMSI_SUSP_TRACE)
        if let Some(path) = omsi_cfg::env::var_os("OMSI_SUSP_TRACE_WINDOW") {
            use std::io::Write;
            static TRACE: std::sync::Mutex<Option<(std::fs::File, f64)>> = std::sync::Mutex::new(None);
            let mut g = TRACE.lock().unwrap_or_else(|e| e.into_inner());
            if g.is_none() {
                if let Ok(mut f) = std::fs::File::create(&path) {
                    let _ = writeln!(f, "t,dt,z,kmh,wheel,compression,rate,load,ground_z");
                    *g = Some((f, 0.0));
                }
            }
            if let (Some((f, t)), Some(rb)) = (g.as_mut(), self.vehicle.rigid.as_ref()) {
                *t += dt as f64;
                for (k, w) in rb.wheels.iter().enumerate() {
                    let _ = writeln!(f, "{:.4},{:.4},{:.4},{:.1},{k},{:.4},{:.3},{:.0},{:.4}", t, dt, rb.position.z, self.vehicle.physics.velocity_kmh(), w.compression, w.compression_rate, w.load, w.ground_z);
                }
            }
        }
        let fired: Vec<String> = std::mem::take(&mut self.vehicle.host.fired_triggers);
        let fired_vars: Vec<(String, Vec<f32>)> = std::mem::take(&mut self.vehicle.host.fired_trigger_vars);
        let fired_files: Vec<(String, String)> =
            std::mem::take(&mut self.vehicle.host.fired_file_triggers);
        for (t, f) in &fired_files {
            log::info!("announcement: {t} -> {f}");
        }
        if let (Some(a), Some(ss)) = (audio, self.sounds.as_mut()) {
            let xf = self.vehicle.world_transform();
            let v = &self.vehicle;
            // the camera decides which `[viewpoint]` entries are heard (the exterior engine
            // samples outside, the rain on the roof in the cab)
            ss.set_inside(inside);
            ss.set_muffled(inside);
            ss.set_listener_vehicle(listener_follows_bus);
            // how open the bus is to the outside (doors, driver's window) for every outside
            // sound heard in it - this bus's own and the traffic's: Omsi.exe reads the
            // player's bus's `Snd_OutsideVol` whatever the camera does (0 when no script
            // writes it)
            omsi_audio::soundset::set_outside_open(Some(v.var("Snd_OutsideVol").unwrap_or(0.0)));
            // (the last time a trigger fired this frame: its sounds start with that moment)
            let at_fire = |t: &str, n: &str| -> Option<f32> {
                let vals = &fired_vars.iter().rev().find(|(k, _)| k.eq_ignore_ascii_case(t))?.1;
                v.var_slot(n).and_then(|i| vals.get(i).copied())
            };
            ss.update_fired(a, &|n| v.var(n), &xf, &fired, &at_fire);
            ss.update_parts(
                a,
                &|n| v.var(n),
                &|i| v.trailers.get(i).map(|t| t.world_transform()),
                &fired,
            );
            for (t, f) in &fired_files {
                ss.play_file_trigger(a, t, f, &|n| v.var(n), &xf);
            }
            if let Some(every) = debug_sound_every() {
                static LAST: std::sync::atomic::AtomicU32 =
                    std::sync::atomic::AtomicU32::new(u32::MAX);
                let bucket = (v.host.clock.time / every as f64) as u32;
                if LAST.swap(bucket, std::sync::atomic::Ordering::Relaxed) != bucket {
                    log::info!(
                        "sound: the player's bus ({} entries, {})",
                        ss.len(),
                        if inside { "inside" } else { "outside" }
                    );
                    for line in ss.report(a, &|n| v.var(n)) {
                        log::info!("  {line}");
                    }
                    for (i, part) in &ss.parts {
                        log::info!("sound: coupled part {i} ({} entries)", part.len());
                        for line in part.report(a, &|n| v.var(n)) {
                            log::info!("  {line}");
                        }
                    }
                }
            }
        }
    }

    /// Attach the vehicle's sound configuration.
    pub(crate) fn load_sounds(&mut self, audio: &omsi_audio::AudioEngine) {
        let def = &self.vehicle.ty.def;
        if let Some(rel) = &def.sound {
            let path = omsi_cfg::resolve_path(def.dir(), rel);
            match omsi_vehicle::SoundCfg::load(&path) {
                Ok(cfg) => {
                    // the bus's own `[next_random]` sounds, chosen by its fleet number
                    let number = self.vehicle.number();
                    let all = cfg.sounds.len();
                    let cfg = cfg.chosen_for(&number);
                    let dir = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
                    let mut ss = omsi_audio::SoundSet::new(audio, &cfg, &dir);
                    log::info!(
                        "sound config {}: {} sounds ({} of {all} for fleet number '{number}')",
                        path.display(),
                        cfg.sounds.len(),
                        cfg.sounds.len()
                    );
                    // the coupled parts' own sound configurations (the engine of a pusher
                    // articulated bus is in its rear section's)
                    for (i, t) in self.vehicle.trailers.iter().enumerate() {
                        let Some(rel) = &t.ty.def.sound else { continue };
                        let path = omsi_cfg::resolve_path(t.ty.def.dir(), rel);
                        match omsi_vehicle::SoundCfg::load(&path) {
                            Ok(cfg) => {
                                let dir = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
                                log::info!(
                                    "sound config of coupled part {i} {}: {} sounds",
                                    path.display(),
                                    cfg.sounds.len()
                                );
                                ss.add_part(i, omsi_audio::SoundSet::new(audio, &cfg.chosen_for(&number), &dir));
                            }
                            Err(e) => log::warn!("{e}"),
                        }
                    }
                    // (the variables its triggered sounds' volume curves read are kept as
                    // they stand when the trigger fires, see `SoundSet::update_fired`)
                    self.vehicle.host.snapshot_triggers = ss.curve_triggers().into_iter().collect();
                    self.sounds = Some(ss);
                }
                Err(e) => log::warn!("{e}"),
            }
        }
    }

    /// The switch under a ray, if any: the `[mouseevent]` mesh the cursor points at.
    ///
    /// The ray through the cursor decides on its own wherever it hits something; only when
    /// it hits nothing are rings of rays around it tried, so that a switch a couple of
    /// pixels wide can still be caught. Letting the ring win over the middle ray (which is
    /// what happened before) meant a switch standing slightly closer than the one actually
    /// under the cursor took the click - the neighbouring toggle flipped instead.
    ///
    /// `spread` is the half-angle of those rings in radians; the window passes the angle
    /// six pixels subtend, so aiming is equally forgiving at any resolution.
    pub(crate) fn pick(&self, origin: DVec3, dir: Vec3, spread: f32) -> Option<usize> {
        let i = pick_in(&self.vehicle, origin, dir, spread)?;
        if self.occlude_controls && self.control_hidden(origin, dir, None, i) {
            return None;
        }
        Some(i)
    }

    /// The page (`[htmltexture]`) under a ray and where it lands on it: the script texture
    /// index and `u`/`v` from 0 to 1 across the page, `v` down from the top.
    pub(crate) fn html_hit(&self, origin: DVec3, dir: Vec3) -> Option<(usize, f32, f32)> {
        pick_html_in(&self.vehicle, origin, dir)
    }

    /// A press, release or move on a page. What the page does with it (`omsi.setVar`,
    /// `omsi.trigger`) reaches the bus at once.
    pub(crate) fn html_pointer(&mut self, page: usize, u: f32, v: f32, kind: omsi_sim::htmltex::PointerKind) -> bool {
        self.vehicle.html_pointer(page, u, v, kind)
    }

    /// The same forgiving pick as `pick`, for the coupled sections of an articulated bus.
    pub(crate) fn pick_trailer(&self, origin: DVec3, dir: Vec3, spread: f32) -> Option<(usize, usize)> {
        let (ti, i) = pick_trailer_in(&self.vehicle, origin, dir, spread)?;
        if self.occlude_controls && self.control_hidden(origin, dir, Some(ti), i) {
            return None;
        }
        Some((ti, i))
    }

    /// Exact surface under a VR pointer, including meshes without a mouse event.
    /// This runs when the mouse moves, not for every headset frame.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn surface_hit(&self, origin: DVec3, dir: Vec3) -> Option<DVec3> {
        let (mut nearest, nearest_control) = self.nearest_hits(origin, dir);
        if nearest_control.is_finite() { nearest = nearest_control; }
        (nearest.is_finite() && nearest < 8.0)
            .then(|| origin + (dir * nearest).as_dvec3())
    }

    /// How far along a ray the bus (any visible mesh, trailers too) is.
    pub(crate) fn body_hit(&self, origin: DVec3, dir: Vec3) -> Option<f32> {
        Some(self.nearest_hits(origin, dir).0).filter(|t| t.is_finite())
    }

    /// Seen from outside: the body of the bus (a wall, a window) is hit before the control
    /// mesh `i` (of coupled part `trailer`, or of the bus itself), so it cannot be reached.
    fn control_hidden(&self, origin: DVec3, dir: Vec3, trailer: Option<usize>, i: usize) -> bool {
        let (ty, position, xf) = match trailer {
            None => (&self.vehicle.ty, self.vehicle.position, self.vehicle.mesh_local_transform(i)),
            Some(ti) => {
                let t = &self.vehicle.trailers[ti];
                (&t.ty, t.position, t.mesh_local_transform(i))
            }
        };
        let Some(&(c, r)) = ty.mesh_bounds.get(i) else {
            return false;
        };
        let dir = dir.normalize_or_zero();
        let o = (origin - position).as_vec3();
        let scale = xf
            .x_axis
            .truncate()
            .length()
            .max(xf.y_axis.truncate().length())
            .max(xf.z_axis.truncate().length());
        let along = (xf.transform_point3(c) - o).dot(dir);
        let nearest = self.nearest_hits(origin, dir).0;
        nearest < along - r * scale - 0.1
    }

    /// The nearest hit of a ray on the bus, and the nearest on a mesh with a mouse event
    /// (infinite: none).
    fn nearest_hits(&self, origin: DVec3, dir: Vec3) -> (f32, f32) {
        let mut nearest = f32::INFINITY;
        let mut nearest_control = f32::INFINITY;
        let vehicle = &self.vehicle;
        let o = (origin - vehicle.position).as_vec3();
        for (i, mesh) in vehicle.ty.meshes.iter().enumerate() {
            if !vehicle.mesh_props[i].visible { continue; }
            let transform = vehicle.mesh_local_transform(i);
            if !ray_may_hit(&vehicle.ty, i, &transform, o, dir, 0.0) { continue; }
            if let Some(t) = omsi_geometry::ray_mesh(o, dir, &mesh.data, &transform) {
                if t > 0.02 {
                    if t < nearest { nearest = t; }
                    if vehicle.ty.model.meshes[mesh.def_index].mouse_event.is_some()
                        && t < nearest_control { nearest_control = t; }
                }
            }
        }
        for trailer in &vehicle.trailers {
            let o = (origin - trailer.position).as_vec3();
            for (i, mesh) in trailer.ty.meshes.iter().enumerate() {
                if !trailer.mesh_props[i].visible { continue; }
                let transform = trailer.mesh_local_transform(i);
                if !ray_may_hit(&trailer.ty, i, &transform, o, dir, 0.0) { continue; }
                if let Some(t) = omsi_geometry::ray_mesh(o, dir, &mesh.data, &transform) {
                    if t > 0.02 {
                        if t < nearest { nearest = t; }
                        if trailer.ty.model.meshes[mesh.def_index].mouse_event.is_some()
                            && t < nearest_control { nearest_control = t; }
                    }
                }
            }
        }
        (nearest, nearest_control)
    }

    /// The part of the bus under a ray, switch or not: `(name, operable)`. Without this the
    /// HUD stayed empty over everything that is not a switch, and there was no way to tell
    /// "this is not a control" from "the cursor is not hitting anything".
    /// The flag beside it: a `[mouseevent]` mesh is under the ray, named or not - Omsi.exe
    /// shows the hand cursor over any of them (0x6f34c0 @0x6f45b4).
    pub(crate) fn hovered_part(&self, origin: DVec3, dir: Vec3, spread: f32) -> (Option<(String, bool)>, bool) {
        if let Some(i) = self.pick(origin, dir, spread) {
            let def = &self.vehicle.ty.model.meshes[self.vehicle.ty.meshes[i].def_index];
            if let Some(ev) = def.mouse_event.clone() {
                // a whole panel that can be dragged into place (the VDV dashboard's
                // `VDV_position`) is no switch to name: its name covered the whole cockpit
                // (the hand still shows over it, a door leaf or the steering column)
                let big = self.vehicle.ty.mesh_bounds.get(i).map(|b| b.1 > 0.45).unwrap_or(false);
                if big {
                    return (None, true);
                }
                return (Some((ev, true)), true);
            }
        }
        if let Some((ti, i)) = self.pick_trailer(origin, dir, spread) {
            let trailer = &self.vehicle.trailers[ti];
            let def = &trailer.ty.model.meshes[trailer.ty.meshes[i].def_index];
            if let Some(ev) = def.mouse_event.clone() {
                return (Some((ev, true)), true);
            }
        }
        (self.hovered_body_part(origin, dir), false)
    }

    /// The mesh of the bus under a ray when it is no switch, by its file's name.
    fn hovered_body_part(&self, origin: DVec3, dir: Vec3) -> Option<(String, bool)> {
        let o = (origin - self.vehicle.position).as_vec3();
        let mut best: Option<(f32, usize)> = None;
        for (i, vm) in self.vehicle.ty.meshes.iter().enumerate() {
            if !self.vehicle.mesh_props[i].visible {
                continue;
            }
            let xf = self.vehicle.mesh_local_transform(i);
            if !ray_may_hit(&self.vehicle.ty, i, &xf, o, dir, 0.0) {
                continue;
            }
            if let Some(t) = omsi_geometry::ray_mesh(o, dir, &vm.data, &xf) {
                if best.map(|(bt, _)| t < bt).unwrap_or(true) {
                    best = Some((t, i));
                }
            }
        }
        let (_, i) = best?;
        let def = &self.vehicle.ty.model.meshes[self.vehicle.ty.meshes[i].def_index];
        let name = std::path::Path::new(&def.file.replace('\\', "/"))
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| def.file.clone());
        Some((name, false))
    }

    pub(crate) fn click(&mut self, origin: DVec3, dir: Vec3, spread: f32) -> Option<usize> {
        self.release();
        if let Some(i) = self.pick(origin, dir, spread) {
            let ev = self.vehicle.ty.model.meshes[self.vehicle.ty.meshes[i].def_index]
                .mouse_event
                .clone()
                .unwrap();
            log::info!("mouse event {ev}");
            let plain = self.vehicle.trigger(&ev);
            self.repair_roller_blind(&ev);
            self.pressed_mesh = Some(i);
            self.press_info = (plain, 0.0);
            self.auto_drag = None;
            return Some(i);
        }
        let (ti, i) = self.pick_trailer(origin, dir, spread)?;
        let ev = self.vehicle.trailers[ti].ty.model.meshes
            [self.vehicle.trailers[ti].ty.meshes[i].def_index]
            .mouse_event
            .clone()
            .unwrap();
        log::info!("trailer mouse event {ev} (part {ti})");
        self.vehicle.trigger(&ev);
        self.repair_roller_blind(&ev);
        self.pressed_trailer_mesh = Some((ti, i));
        Some(i)
    }

    /// Mouse moved with the button down on a switch: OMSI fires `<event>_drag` with the
    /// movement in `mouse_x` / `mouse_y`, which is how the rotary knobs, the sun blind, the
    /// driver's window and the ignition key are operated.
    pub(crate) fn drag(&mut self, dx: f32, dy: f32) {
        let (ev, trailer) = if let Some(i) = self.pressed_mesh {
            let Some(ev) = self.vehicle.ty.model.meshes[self.vehicle.ty.meshes[i].def_index]
                .mouse_event
                .clone()
            else {
                return;
            };
            (ev, false)
        } else if let Some((ti, i)) = self.pressed_trailer_mesh {
            let Some(ev) = self.vehicle.trailers[ti].ty.model.meshes
                [self.vehicle.trailers[ti].ty.meshes[i].def_index]
                .mouse_event
                .clone()
            else {
                return;
            };
            (ev, true)
        } else {
            return;
        };
        let name = format!("{ev}_drag");
        self.press_info.1 += dx.abs() + dy.abs();
        self.vehicle.host.mouse = (dx, dy);
        self.vehicle.trigger(&name);
        self.repair_roller_blind(&name);
        self.vehicle.host.mouse = (0.0, 0.0);
        let _ = trailer;
    }

    /// One notch of the mouse wheel over a switch: `<event>_drag` with the notch as the
    /// movement, and the `_off` the script expects when the hand lets go again.
    pub(crate) fn wheel(&mut self, origin: DVec3, dir: Vec3, spread: f32, amount: f32) {
        let Some(i) = self.pick(origin, dir, spread) else {
            return;
        };
        let Some(ev) = self.vehicle.ty.model.meshes[self.vehicle.ty.meshes[i].def_index]
            .mouse_event
            .clone()
        else {
            return;
        };
        // vertical, like the wheel itself: that is the axis the knobs, the sun blind and
        // the ignition key read (mouse_x belongs to the driver's window and the light
        // rotary, which stay a drag)
        self.vehicle.host.mouse = (0.0, amount);
        self.vehicle.trigger(&format!("{ev}_drag"));
        self.repair_roller_blind(&format!("{ev}_drag"));
        self.vehicle.host.mouse = (0.0, 0.0);
        self.vehicle.trigger(&format!("{ev}_off"));
    }

    /// The left button let go while the right one is held: Omsi.exe sends no `_off`, so a
    /// momentary switch stays where the hand left it (a pedal held down for the steering
    /// column's adjustment, #769) until it is clicked and let go again.
    pub(crate) fn release_keeping(&mut self) {
        if let Some(i) = self.pressed_mesh.take() {
            let def = &self.vehicle.ty.model.meshes[self.vehicle.ty.meshes[i].def_index];
            log::info!("mouse event {:?}: let go with the right button held, the switch stays", def.mouse_event);
        }
        self.pressed_trailer_mesh = None;
    }

    pub(crate) fn release(&mut self) {
        if let Some(i) = self.pressed_mesh.take() {
            let ty = self.vehicle.ty.clone();
            let def = &ty.model.meshes[ty.meshes[i].def_index];
            if let Some(ev) = def.mouse_event.clone() {
                self.vehicle.trigger(&format!("{ev}_off"));
                // a plain click on a control the scripts only know as a drag: dragged to its
                // other end (a click opened the NL/NG driver's door by the mouse's jitter
                // and a second click never shut it again)
                let anim = def.animations.first().map(|a| a.variable.clone()).filter(|v| !v.trim().is_empty() && v.trim().parse::<f32>().is_err());
                if let (false, true, Some(var)) = (self.press_info.0, self.press_info.1 < 4.0, anim) {
                    let now = self.vehicle.var(&var).unwrap_or(0.0);
                    let target = if now > 0.5 { 0.0 } else { 1.0 };
                    log::info!("mouse event {ev}: a click on a drag control, {var} {now:.2} -> {target}");
                    self.auto_drag = Some(AutoDrag { ev, var, target, axis: 0, sign: if target > now { -1.0 } else { 1.0 }, last: now, stalled: 0, t: 0.0 });
                }
            }
        }
        if let Some((ti, i)) = self.pressed_trailer_mesh.take() {
            if let Some(ev) = self.vehicle.trailers[ti].ty.model.meshes
                [self.vehicle.trailers[ti].ty.meshes[i].def_index]
                .mouse_event
                .clone()
            {
                self.vehicle.trigger(&format!("{ev}_off"));
            }
        }
    }
}

/// See `Player::auto_drag`.
#[derive(Debug, Clone)]
pub(crate) struct AutoDrag {
    ev: String,
    var: String,
    target: f32,
    /// The mouse axis (0 x, 1 y) and direction that move it towards the target.
    axis: usize,
    sign: f32,
    last: f32,
    stalled: u32,
    t: f32,
}

impl Player {
    /// Drag a clicked drag-only control (see `auto_drag`) a step towards its other end.
    pub(crate) fn tick_auto_drag(&mut self, dt: f32) {
        let Some(mut a) = self.auto_drag.take() else { return };
        let now = self.vehicle.var(&a.var).unwrap_or(a.last);
        a.t += dt;
        if (now - a.target).abs() < 0.03 || a.t > 2.5 {
            self.vehicle.trigger(&format!("{}_off", a.ev));
            return;
        }
        // moved away from the target, or not at all: another direction, then the other axis
        let towards = (a.target - a.last).signum();
        let moved = now - a.last;
        if a.t > 0.05 {
            if moved * towards < -1e-4 {
                a.sign = -a.sign;
            } else if moved.abs() < 1e-4 {
                a.stalled += 1;
                if a.stalled == 3 {
                    a.sign = -a.sign;
                } else if a.stalled == 6 {
                    a.axis = 1 - a.axis;
                } else if a.stalled == 9 {
                    a.sign = -a.sign;
                } else if a.stalled > 12 {
                    self.vehicle.trigger(&format!("{}_off", a.ev));
                    return;
                }
            } else {
                a.stalled = 0;
            }
        }
        a.last = now;
        let step = 900.0 * dt.min(0.05) * a.sign;
        self.vehicle.host.mouse = if a.axis == 0 { (step, 0.0) } else { (0.0, step) };
        let exists = self.vehicle.trigger(&format!("{}_drag", a.ev));
        self.vehicle.host.mouse = (0.0, 0.0);
        if exists {
            self.auto_drag = Some(a);
        }
    }

    /// `inside`: the camera is one of the vehicle's interior cameras. `[viewpoint]` bits:
    /// 1 = visible from outside, 2 = visible from inside, 4 = visible on AI vehicles; 0 = always.
    pub(crate) fn sync_transforms(&mut self, renderer: &Renderer, scene: &mut Scene, inside: bool) {
        sync_vehicle_transforms(renderer, scene, &mut self.vehicle, &mut self.render, &mut self.trailer_renders, inside);
    }

    /// Pose and place the driver at the wheel; `show` false hides the figure (the `driver`
    /// setting off), `mirror_only` keeps it to the mirrors (the cab view).
    pub(crate) fn sync_driver(&mut self, renderer: &Renderer, scene: &mut Scene, dt: f32, show: bool, mirror_only: bool) {
        self.sync_driver_hands(renderer, scene, dt, show, mirror_only, false);
    }

    /// As [`Player::sync_driver`], with the driver's hands shown in the cab view or not
    /// (Settings → "Driver's hands in the cab view").
    pub(crate) fn sync_driver_hands(&mut self, renderer: &Renderer, scene: &mut Scene, dt: f32, show: bool, mirror_only: bool, hands: bool) {
        if let Some(d) = self.driver.as_mut() {
            d.show_hands_in_cab = hands;
            d.update(renderer, scene, &self.vehicle, &self.render, dt, show, mirror_only);
        }
    }

    /// The driver's chosen camera as it is fixed in the bus (before the bus's own motion), turned
    /// by the look and the steering: what a glide between two cameras mixes.
    pub(crate) fn driver_local(&self, look: (f32, f32)) -> Option<omsi_vehicle::Camera> {
        // (no glide onto a coupled part's camera: it is not in the front's frame)
        if self.trailer_driver_camera().is_some() {
            return None;
        }
        let def = &self.vehicle.ty.def;
        let n = def.cameras_driver.len().max(1);
        let c = def.cameras_driver.get((def.camera_std + self.cam_choice.0) % n).or(def.cameras_driver.first())?;
        Some(omsi_vehicle::Camera {
            yaw: c.yaw + look.0 + self.steer_look,
            pitch: (c.pitch + look.1).clamp(-89.0, 89.0),
            ..c.clone()
        })
    }

    /// `driver_local`'s camera in the world, with the bus's pitch and bank and the head on it
    /// (the same as `camera_look` makes of the driver's camera).
    pub(crate) fn driver_world(&self, turned: &omsi_vehicle::Camera) -> Camera {
        let (eye, yaw, pitch, roll) = self.vehicle.camera_world_full(turned);
        let eye = eye + self.vehicle.body_rotation().transform_vector3(self.head_offset() + self.seat).as_dvec3();
        Camera { position: eye, yaw, pitch: pitch.clamp(-89.0, 89.0), roll, fov_deg: turned.fov, near: 0.1, far: 6000.0 }
    }

    /// Where a camera in the world (the walker's eyes) is in the bus's frame.
    pub(crate) fn local_of_world(&self, cam: &Camera) -> omsi_vehicle::Camera {
        let inv = self.vehicle.body_rotation().inverse();
        let pos = inv.transform_vector3((cam.position - self.vehicle.position).as_vec3());
        let f = inv.transform_vector3(cam.forward());
        omsi_vehicle::Camera {
            pos: pos.to_array(),
            yaw: f.x.atan2(f.y).to_degrees(),
            pitch: f.z.clamp(-1.0, 1.0).asin().to_degrees(),
            fov: cam.fov_deg,
            ..Default::default()
        }
    }

    pub(crate) fn camera(&self, view: &str, fallback: &Camera) -> Camera {
        self.camera_look(view, fallback, (0.0, 0.0), ORBIT_DEFAULT)
    }

    /// The outside camera must not go through walls or under the road: it stands short of
    /// the first building, wall or canopy between it and the point it orbits, and above the
    /// ground (see `camera_arm`). `dt` 0 is a single picture: the arm is where it would
    /// settle, without easing.
    pub(crate) fn camera_clipped(
        &mut self,
        mut cam: Camera,
        world: &scene::World,
        dist: f32,
        dt: f32,
    ) -> Camera {
        let def = &self.vehicle.ty.def;
        let c = def.camera_outside_center;
        let centre = orbit_pivot(self.vehicle.position, self.vehicle.heading, c);
        let want = dist.clamp(ORBIT_MIN, ORBIT_MAX);
        let back = -cam.forward().as_dvec3().normalize_or_zero();
        if back.length_squared() < 0.5 {
            return cam;
        }
        let right = cam.right().as_dvec3().normalize_or_zero();
        let up = back.cross(right).normalize_or_zero();
        let t0 = Instant::now();
        let free = camera_arm::free_length(world, centre, back, right, up, want as f64) as f32;
        let len = if dt <= 0.0 {
            self.arm.reset();
            free
        } else {
            self.arm.update(want, free, centre, dt)
        };
        if camera_arm::debug_level() >= 2 {
            log::info!(
                "camera arm: yaw {:.1} want {want:.2} free {free:.2} arm {len:.2} ({:.3} ms)",
                cam.yaw,
                t0.elapsed().as_secs_f64() * 1000.0
            );
        }
        cam.position = centre + back * len as f64;
        cam
    }

    /// How many passenger cameras the bus has, its coupled parts' included.
    pub(crate) fn pax_camera_count(&self) -> usize {
        self.vehicle.ty.def.cameras_pax.len() + self.vehicle.trailers.iter().map(|t| t.ty.def.cameras_pax.len()).sum::<usize>()
    }

    /// How many driver cameras the bus has, its coupled parts' included: Omsi.exe's
    /// interior-camera keys go on from the last of one part's into the next part's
    /// (0x706278 @0x7067d1, @0x706a3c).
    pub(crate) fn driver_camera_count(&self) -> usize {
        self.vehicle.ty.def.cameras_driver.len() + self.vehicle.trailers.iter().map(|t| t.ty.def.cameras_driver.len()).sum::<usize>()
    }

    /// The driver camera chosen past the front's own: the coupled part it is on and the
    /// camera (None while one of the front's is chosen).
    pub(crate) fn trailer_driver_camera(&self) -> Option<(&omsi_sim::vehicle::TrailerPart, &omsi_vehicle::Camera)> {
        let front = self.vehicle.ty.def.cameras_driver.len();
        let mut k = self.cam_choice.0 % self.driver_camera_count().max(1);
        if k < front {
            return None;
        }
        k -= front;
        for t in &self.vehicle.trailers {
            if let Some(c) = t.ty.def.cameras_driver.get(k) {
                return Some((t, c));
            }
            k -= t.ty.def.cameras_driver.len();
        }
        None
    }

    /// `look`: yaw/pitch the player has turned the head (or the orbit) by; `dist`: how far
    /// the outside camera sits from the vehicle.
    pub(crate) fn camera_look(&self, view: &str, fallback: &Camera, look: (f32, f32), dist: f32) -> Camera {
        let def = &self.vehicle.ty.def;
        // `mirror<n>`: what the n-th mirror's camera sees, as it is drawn into the mirror's
        // picture (a check of the mirrors against OMSI's own `reflexion<n>.bmp`)
        if let Some(c) = view.strip_prefix("mirror").and_then(|n| n.parse::<usize>().ok()).and_then(|n| def.cameras_reflexion.get(n)) {
            let k = def.cameras_reflexion.iter().position(|x| std::ptr::eq(x, c)).unwrap_or(0);
            let aimed = crate::camera_util::mirror_view(&self.vehicle, &crate::camera_util::adjusted(c, self.mirror_shifts.get(k).copied().unwrap_or([0.0; 3]), self.mirror_fovs.get(k).copied().unwrap_or(0.0)), crate::camera_util::driver_eye(self), self.mirror_offsets.get(k).copied().unwrap_or([0.0; 2]));
            let (eye, yaw, pitch, roll) = self.vehicle.camera_world_full(&aimed);
            return Camera { position: eye, yaw, pitch, roll, fov_deg: if c.fov > 1.0 { c.fov } else { 50.0 }, near: 0.1, far: 450.0 };
        }
        let cam = match view {
            "driver" => {
                // (a coupled part's driver camera, on that part's body)
                if let Some((t, c)) = self.trailer_driver_camera() {
                    let turned = omsi_vehicle::Camera { yaw: c.yaw + look.0 + self.steer_look, pitch: (c.pitch + look.1).clamp(-89.0, 89.0), ..c.clone() };
                    let (eye, yaw, pitch, roll) = t.camera_world_full(&turned);
                    return Camera { position: eye, yaw, pitch: pitch.clamp(-89.0, 89.0), roll, fov_deg: c.fov, near: 0.1, far: 6000.0 };
                }
                let n = def.cameras_driver.len().max(1);
                def.cameras_driver
                    .get((def.camera_std + self.cam_choice.0) % n)
                    .or(def.cameras_driver.first())
            }
            "pax" => {
                // the passenger cameras of every part of the bus, the front's first: an
                // articulated bus's rear section brings its own in its `.bus`
                let n = self.pax_camera_count().max(1);
                let k = self.cam_choice.1 % n;
                match def.cameras_pax.get(k) {
                    Some(c) => Some(c),
                    None => {
                        let mut k = k - def.cameras_pax.len();
                        let mut found = None;
                        for t in &self.vehicle.trailers {
                            if let Some(c) = t.ty.def.cameras_pax.get(k) {
                                // (on the part's body like the front's: `dist`, pitch and roll)
                                let turned = omsi_vehicle::Camera { yaw: c.yaw + look.0, pitch: (c.pitch + look.1).clamp(-89.0, 89.0), ..c.clone() };
                                found = Some((t.camera_world_full(&turned), c.fov));
                                break;
                            }
                            k -= t.ty.def.cameras_pax.len();
                        }
                        if let Some(((eye, yaw, pitch, roll), fov)) = found {
                            return Camera { position: eye, yaw, pitch: pitch.clamp(-89.0, 89.0), roll, fov_deg: fov, near: 0.1, far: 6000.0 };
                        }
                        def.cameras_pax.first()
                    }
                }
            }
            _ => None,
        };
        match cam {
            Some(c) => {
                // inside: the head turns, the seat does not move. The camera hangs on the
                // body as Omsi.exe hangs its driver's and passengers' cameras (0x7cf82c ->
                // 0x7edfd0, the vehicle's own matrix): it pitches and rolls with the bus, the
                // look turned in the bus's frame. Kept level, the view stood still while the
                // cab rocked about it - the "boat" (the body's own motion matches Omsi's).
                let turned = omsi_vehicle::Camera { yaw: c.yaw + look.0 + if view == "driver" { self.steer_look } else { 0.0 }, pitch: (c.pitch + look.1).clamp(-89.0, 89.0), ..c.clone() };
                let (eye, yaw, pitch, roll) = self.vehicle.camera_world_full(&turned);
                let eye = if view == "driver" { eye + self.vehicle.body_rotation().transform_vector3(self.head_offset() + self.seat).as_dvec3() } else { eye };
                // near 0.1 as in Omsi.exe (every view, 0x6f6aa7); with the reversed float
                // depth buffer it costs no precision out at 6 km
                Camera {
                    position: eye,
                    yaw,
                    pitch: pitch.clamp(-89.0, 89.0),
                    roll,
                    fov_deg: c.fov,
                    near: 0.1,
                    far: 6000.0,
                }
            }
            None => {
                // outside view: an orbit around the vehicle
                let c = def.camera_outside_center;
                let center = orbit_pivot(self.vehicle.position, self.vehicle.heading, c);
                let mut cam = Camera {
                    position: center,
                    yaw: self.vehicle.heading as f32 - 35.0 + look.0,
                    pitch: (-15.0 + look.1).clamp(-85.0, 85.0),
                    roll: 0.0,
                    fov_deg: fallback.fov_deg,
                    near: 0.1,
                    far: 6000.0,
                };
                cam.position =
                    center - (cam.forward() * dist.clamp(ORBIT_MIN, ORBIT_MAX)).as_dvec3();
                cam
            }
        }
    }
}

/// Outside-camera pivot: the `.bus` centre rotated by heading alone. Body pitch
/// and bank (suspension bounce, cornering roll) would swing the camera if they
/// reached the pivot; the view only ever yaws with the bus.
pub(crate) fn orbit_pivot(position: DVec3, heading_deg: f64, center: [f32; 3]) -> DVec3 {
    position
        + glam::Mat4::from_rotation_z((-(heading_deg as f32)).to_radians())
            .transform_point3(Vec3::new(center[0], center[1], center[2]))
            .as_dvec3()
}

/// Put a vehicle's meshes where its state says (animations, visibility, lights, the
/// matrix textures) - the player's bus, and the launcher's showroom bus.
pub(crate) fn sync_vehicle_transforms(
    renderer: &Renderer,
    scene: &mut Scene,
    vehicle: &mut omsi_sim::VehicleInstance,
    render: &mut scene::VehicleRender,
    trailer_renders: &mut [scene::VehicleRender],
    inside: bool,
) {
    scene::sync_vehicle_materials(renderer, scene, vehicle, render);
    scene::sync_skinned(
        renderer,
        scene,
        vehicle,
        render,
        trailer_renders,
    );
    for (i, inst) in render.instances.iter().enumerate() {
        renderer.set_transform(
            scene,
            *inst,
            vehicle.position,
            vehicle.mesh_local_transform(i),
        );
        let p = &vehicle.mesh_props[i];
        let def = &vehicle.ty.model.meshes[vehicle.ty.meshes[i].def_index];
        let vp = def.viewpoint;
        let vp_ok = vp == 0 || (inside && vp & 2 != 0) || (!inside && vp & 1 != 0);
        // the outside of the bus seen from the cab: not in the picture, but in the mirrors,
        // which look at the bus from outside (its flanks were missing from them)
        let mirror = inside && vp & 1 != 0 && vp & 2 == 0;
        let mut alpha = p.slot_alpha.clone();
        for (slot, mat) in scene.instances[*inst].materials.iter().enumerate() {
            if scene
                .materials
                .get(*mat)
                .is_some_and(|m| m.alpha == omsi_render::AlphaMode::Opaque)
            {
                if let Some(a) = alpha.get_mut(slot) {
                    *a = 1.0;
                }
            }
        }
        // (a shadow blob is left out by the renderer while it draws the sun's shadows)
        renderer.set_params(scene, *inst, &alpha, p.visible && (vp_ok || mirror), &p.slot_uv);
        renderer.set_mirror_only(scene, *inst, mirror);
        renderer.set_slot_light(scene, *inst, &p.slot_light);
        renderer.set_slot_night(scene, *inst, &p.slot_night);
        renderer.set_interior(scene, *inst, p.interior);
    }
    for (t, r) in vehicle.trailers.iter().zip(trailer_renders.iter()) {
        for (i, inst) in r.instances.iter().enumerate() {
            renderer.set_transform(scene, *inst, t.position, t.mesh_local_transform(i));
            let p = &t.mesh_props[i];
            let def = &t.ty.model.meshes[t.ty.meshes[i].def_index];
            let vp = def.viewpoint;
            let vp_ok = vp == 0 || (inside && vp & 2 != 0) || (!inside && vp & 1 != 0);
            let mirror = inside && vp & 1 != 0 && vp & 2 == 0;
            let mut alpha = p.slot_alpha.clone();
            for (slot, mat) in scene.instances[*inst].materials.iter().enumerate() {
                if scene
                    .materials
                    .get(*mat)
                    .is_some_and(|m| m.alpha == omsi_render::AlphaMode::Opaque)
                {
                    if let Some(a) = alpha.get_mut(slot) {
                        *a = 1.0;
                    }
                }
            }
            renderer.set_params(scene, *inst, &alpha, p.visible && (vp_ok || mirror), &p.slot_uv);
            renderer.set_mirror_only(scene, *inst, mirror);
            renderer.set_slot_light(scene, *inst, &p.slot_light);
            renderer.set_slot_night(scene, *inst, &p.slot_night);
            renderer.set_interior(scene, *inst, p.interior);
        }
    }
    scene::sync_vehicle_textures(renderer, scene, vehicle, render, &mut { usize::MAX });
    let mut trailers = std::mem::take(&mut vehicle.trailers);
    for (t, r) in trailers.iter_mut().zip(trailer_renders.iter_mut()) {
        scene::sync_vehicle_part(renderer, scene, &vehicle, t, r);
    }
    vehicle.trailers = trailers;
}

pub(crate) fn pick_in(vehicle: &omsi_sim::VehicleInstance, origin: DVec3, dir: Vec3, spread: f32) -> Option<usize> {
    let o = (origin - vehicle.position).as_vec3();
    let right = Vec3::new(-dir.y, dir.x, 0.0).normalize_or_zero();
    let up = dir.cross(right).normalize_or_zero();
    let mut rings: Vec<Vec<Vec3>> = vec![vec![dir]];
    if spread > 0.0 {
        for ring in 1..=2 {
            let r = spread * ring as f32;
            let n = 8 * ring;
            rings.push(
                (0..n)
                    .map(|k| {
                        let a = k as f32 / n as f32 * std::f32::consts::TAU;
                        (dir + right * (a.cos() * r) + up * (a.sin() * r)).normalize()
                    })
                    .collect(),
            );
        }
    }
    // The same broad-phase applies to every ring. A large cockpit can contain
    // hundreds of meshes; calculating their posed transforms three times made
    // hovering over its controls needlessly expensive.
    let candidates: Vec<(usize, glam::Mat4, Vec<u32>)> = vehicle.ty.meshes.iter().enumerate().filter_map(|(i, vm)| {
        if vehicle.ty.model.meshes[vm.def_index].mouse_event.is_none() || !vehicle.mesh_props[i].visible {
            return None;
        }
        let xf = vehicle.mesh_local_transform(i);
        if !ray_may_hit(&vehicle.ty, i, &xf, o, dir, spread * 2.2) {
            return None;
        }
        let tris = omsi_geometry::cone_triangles(o, dir, spread * 2.0 + 1e-4, &vm.data, &xf);
        (!tris.is_empty()).then_some((i, xf, tris))
    }).collect();
    for dirs in &rings {
        let mut best: Option<(f32, usize)> = None;
        for (i, xf, tris) in &candidates {
            let (i, xf) = (*i, *xf);
            let vm = &vehicle.ty.meshes[i];
            for d in dirs {
                if let Some(t) = omsi_geometry::ray_triangles(o, *d, &vm.data, &xf, tris) {
                    if best.map(|(bt, _)| t < bt).unwrap_or(true) {
                        best = Some((t, i));
                    }
                }
            }
        }
        if let Some((_, i)) = best {
            return Some(i);
        }
    }
    None
}

/// The page of an `[htmltexture]` a ray lands on: its script texture index and the place on it
/// (`u`/`v` from 0 to 1, `v` down from the top), for the nearest such surface. The nearest
/// triangle of a mesh decides: a bezel of the same mesh in front of the screen takes the click
/// away from it.
pub(crate) fn pick_html_in(vehicle: &omsi_sim::VehicleInstance, origin: DVec3, dir: Vec3) -> Option<(usize, f32, f32)> {
    if vehicle.html_textures.is_empty() {
        return None;
    }
    let pages: Vec<usize> = vehicle.html_textures.iter().map(|t| t.script_index).collect();
    let o = (origin - vehicle.position).as_vec3();
    let mut best: Option<(f32, usize, f32, f32)> = None;
    for (i, vm) in vehicle.ty.meshes.iter().enumerate() {
        if !vehicle.mesh_props[i].visible {
            continue;
        }
        let def = &vehicle.ty.model.meshes[vm.def_index];
        let shows_page = |n: Option<i32>| n.is_some_and(|n| pages.contains(&(n.max(0) as usize)));
        if !def.materials.iter().any(|m| shows_page(m.use_script_texture)) {
            continue;
        }
        let xf = vehicle.mesh_local_transform(i);
        if !ray_may_hit(&vehicle.ty, i, &xf, o, dir, 0.0) {
            continue;
        }
        let Some(hit) = omsi_geometry::ray_mesh_hit(o, dir, &vm.data, &xf) else {
            continue;
        };
        // the page the hit material slot shows (a slot that shows none is in the way)
        let slot = vm.data.slot_of(hit.index) as usize;
        let page = def
            .materials
            .iter()
            .filter(|m| omsi_sim::vehicle::override_slot(&vm.materials, m) == Some(slot))
            .find_map(|m| m.use_script_texture)
            .map(|n| n.max(0) as usize)
            .filter(|n| pages.contains(n));
        let Some(page) = page else { continue };
        if best.is_none_or(|b| hit.t < b.0) {
            best = Some((hit.t, page, hit.uv.x.clamp(0.0, 1.0), hit.uv.y.clamp(0.0, 1.0)));
        }
    }
    best.map(|(_, page, u, v)| (page, u, v))
}

/// The same forgiving pick as `pick`, but for the coupled sections of an articulated
/// bus.  The old picker only searched the lead vehicle, so GN92's rear door opener and
/// every other button in the second section could never receive a click.
pub(crate) fn pick_trailer_in(vehicle: &omsi_sim::VehicleInstance, origin: DVec3, dir: Vec3, spread: f32) -> Option<(usize, usize)> {
    let right = Vec3::new(-dir.y, dir.x, 0.0).normalize_or_zero();
    let up = dir.cross(right).normalize_or_zero();
    let mut rings: Vec<Vec<Vec3>> = vec![vec![dir]];
    if spread > 0.0 {
        for ring in 1..=2 {
            let r = spread * ring as f32;
            let n = 8 * ring;
            rings.push(
                (0..n)
                    .map(|k| {
                        let a = k as f32 / n as f32 * std::f32::consts::TAU;
                        (dir + right * (a.cos() * r) + up * (a.sin() * r)).normalize()
                    })
                    .collect(),
            );
        }
    }
    let candidates: Vec<(usize, usize, glam::Mat4, Vec<u32>)> = vehicle.trailers.iter().enumerate().flat_map(|(ti, trailer)| {
        let o = (origin - trailer.position).as_vec3();
        trailer.ty.meshes.iter().enumerate().filter_map(move |(i, vm)| {
            if trailer.ty.model.meshes[vm.def_index].mouse_event.is_none() || !trailer.mesh_props[i].visible {
                return None;
            }
            let xf = trailer.mesh_local_transform(i);
            if !ray_may_hit(&trailer.ty, i, &xf, o, dir, spread * 2.2) {
                return None;
            }
            let tris = omsi_geometry::cone_triangles(o, dir, spread * 2.0 + 1e-4, &vm.data, &xf);
            (!tris.is_empty()).then_some((ti, i, xf, tris))
        })
    }).collect();
    for dirs in &rings {
        let mut best: Option<(f32, usize, usize)> = None;
        for (ti, i, xf, tris) in &candidates {
            let (ti, i, xf) = (*ti, *i, *xf);
            let trailer = &vehicle.trailers[ti];
            let o = (origin - trailer.position).as_vec3();
            let vm = &trailer.ty.meshes[i];
            for d in dirs {
                if let Some(t) = omsi_geometry::ray_triangles(o, *d, &vm.data, &xf, tris) {
                    if best.map(|(bt, _, _)| t < bt).unwrap_or(true) {
                        best = Some((t, ti, i));
                    }
                }
            }
        }
        if let Some((_, ti, i)) = best {
            return Some((ti, i));
        }
    }
    None
}

/// Mouse steering switched off: the wheel stays where the mouse left it and the keys go on
/// from there, as in OMSI, where the mouse and the keys turn the one wheel (#184). The
/// keys' own position was held at the middle while the mouse steered, and the wheel sprang
/// back to it.
pub(crate) fn keep_wheel(p: Option<&mut Player>) {
    if let Some(p) = p {
        p.axes.steering = p.vehicle.physics.controls.steering;
        p.axes.centering = false;
    }
}

/// OMSI's mouse steering (Omsi.exe 0x6f4284..0x6f447b): the cursor's place across the whole
/// window is the steering from full left to full right lock, divided by the speed in tens of
/// km/h once the bus is faster than 10 km/h (going backwards counts as standing). At 50 km/h
/// the same movement of the hand turns the wheels a fifth as far: the wheel "gets heavier".
pub(crate) fn mouse_steering(cursor_x: f32, width: f32, kmh: f32) -> f32 {
    let x = (2.0 * cursor_x / width.max(1.0) - 1.0).clamp(-1.0, 1.0);
    x / (kmh / 10.0).max(1.0)
}

/// A mouse pedal following the cursor, `k` of the way left behind each frame. The last bit
/// is snapped: in f32 the easing stops one step short of the target (1 - 6e-8 at 60 fps), and
/// the stock gearbox scripts kick down only at a throttle of exactly 1 - the cursor at the top
/// edge never gave it.
pub(crate) fn mouse_pedal(current: f32, target: f32, k: f32) -> f32 {
    let v = target + (current - target) * k;
    if (v - target).abs() < 1e-4 { target } else { v }
}

#[cfg(test)]
mod kept_indicator_tests {
    use super::kept_indicator;

    #[test]
    fn a_kept_indicator_is_put_back_when_the_script_cancels_it() {
        // the script's own cancelling stands by default
        assert_eq!(kept_indicator(true, Some(1.0), Some(0.0)), None);
        // kept: left and right come back, the hazard lever and a change of side stay
        assert_eq!(kept_indicator(false, Some(1.0), Some(0.0)), Some(1.0));
        assert_eq!(kept_indicator(false, Some(2.0), Some(0.0)), Some(2.0));
        assert_eq!(kept_indicator(false, Some(1.0), Some(2.0)), None);
        assert_eq!(kept_indicator(false, Some(0.0), Some(0.0)), None);
        assert_eq!(kept_indicator(false, None, Some(0.0)), None);
    }
}

#[cfg(test)]
mod orbit_pivot_tests {
    use super::orbit_pivot;
    use glam::DVec3;

    #[test]
    fn pivot_yaws_with_the_bus_and_ignores_body_attitude() {
        // heading 0: the centre passes through unrotated.
        let p = orbit_pivot(DVec3::new(10.0, 20.0, 5.0), 0.0, [0.0, 0.0, 1.2]);
        assert!((p.x - 10.0).abs() < 1e-6 && (p.y - 20.0).abs() < 1e-6);
        assert!((p.z - 6.2).abs() < 1e-6, "{p:?}");
        // heading 90: the forward offset swings sideways, height untouched
        // (a body_rotation pivot would also tilt it with pitch/bank).
        let q = orbit_pivot(DVec3::ZERO, 90.0, [1.0, 2.0, 1.2]);
        assert!((q.x - 2.0).abs() < 1e-5 && (q.y + 1.0).abs() < 1e-5, "{q:?}");
        assert!((q.z - 1.2).abs() < 1e-6, "{q:?}");
    }
}

#[cfg(test)]
mod mouse_tests {
    use super::{mouse_pedal, mouse_steering};

    #[test]
    fn the_mouse_pedal_reaches_the_floor() {
        for fps in [30.0f32, 60.0, 144.0] {
            let k = (-(1.0 / fps) / 0.06f32).exp();
            let (mut t, mut b) = (0.0, 1.0);
            for _ in 0..(fps as usize) {
                t = mouse_pedal(t, 1.0, k);
                b = mouse_pedal(b, 0.0, k);
            }
            // exactly: the kickdown compares the throttle with 1
            assert_eq!(t, 1.0, "{fps} fps");
            assert_eq!(b, 0.0, "{fps} fps");
        }
        // on the way it still eases
        let k = (-(1.0 / 60.0f32) / 0.06).exp();
        let t = mouse_pedal(0.0, 1.0, k);
        assert!(t > 0.2 && t < 0.3, "{t}");
    }

    #[test]
    fn the_mouse_steers_less_the_faster_the_bus() {
        let w = 1600.0;
        // standing and slow: the whole width is the whole lock
        assert_eq!(mouse_steering(0.0, w, 0.0), -1.0);
        assert_eq!(mouse_steering(1600.0, w, 5.0), 1.0);
        assert_eq!(mouse_steering(800.0, w, 0.0), 0.0);
        assert!((mouse_steering(1200.0, w, 10.0) - 0.5).abs() < 1e-6);
        // faster: divided by the speed in tens of km/h
        assert!((mouse_steering(1200.0, w, 50.0) - 0.1).abs() < 1e-6);
        assert!((mouse_steering(1600.0, w, 100.0) - 0.1).abs() < 1e-6);
        // backwards like standing
        assert!((mouse_steering(1200.0, w, -20.0) - 0.5).abs() < 1e-6);
        // it moves smoothly: no step anywhere across the window
        let mut last = mouse_steering(0.0, w, 30.0);
        for px in 1..=1600 {
            let s = mouse_steering(px as f32, w, 30.0);
            assert!((s - last).abs() < 0.001, "step at {px}");
            last = s;
        }
    }
}

/// The line of a depot route: the file's line column, or - when that is empty or a placeholder
/// such as "XXX" - the line of its code (`code` = line x 100 + route).
fn route_line(t: &omsi_vehicle::hof::InfoTrip) -> String {
    let raw = t.line.trim();
    let code = omsi_cfg::parse_i32(&t.code);
    if (raw.is_empty() || raw.chars().all(|c| c == 'x' || c == 'X')) && code >= 100 {
        (code / 100).to_string()
    } else {
        raw.to_string()
    }
}
#[cfg(test)]
mod preset_tests {
    use super::fallback_action;
    use omsi_sim::EngineAction as A;
    use winit::keyboard::KeyCode;

    #[test]
    fn plain_left_right_steer_only_with_the_arrows_preset() {
        assert_eq!(fallback_action(KeyCode::ArrowLeft, "simple"), None);
        assert_eq!(fallback_action(KeyCode::ArrowRight, "simple"), None);
        assert_eq!(fallback_action(KeyCode::ArrowUp, "simple"), Some(A::Throttle));
        assert_eq!(fallback_action(KeyCode::KeyA, "simple"), Some(A::SteeringLeft));
        assert_eq!(fallback_action(KeyCode::ArrowLeft, "arrows"), Some(A::SteeringLeft));
        assert_eq!(fallback_action(KeyCode::ArrowRight, "arrows"), Some(A::SteeringRight));
    }
}

#[cfg(test)]
mod indicator_tests {
    use super::indicator_toggle_action;

    #[test]
    fn repeated_presses_switch_each_side_on_then_off() {
        for (side, on) in [(1, "blinker_left_set"), (2, "blinker_right_set")] {
            let mut state = 0;
            assert_eq!(indicator_toggle_action(&mut state, None, side), on);
            assert_eq!(indicator_toggle_action(&mut state, None, side), "blinker_off");
            assert_eq!(state, 0);
        }
    }

    #[test]
    fn changing_side_and_automatic_cancellation_use_the_current_lever() {
        let mut state = 1;
        assert_eq!(indicator_toggle_action(&mut state, Some(1), 2), "blinker_right_set");
        assert_eq!(indicator_toggle_action(&mut state, Some(0), 2), "blinker_right_set");
        assert_eq!(indicator_toggle_action(&mut state, Some(2), 2), "blinker_off");
    }

    #[test]
    fn hazards_keep_their_dedicated_toggle_trigger() {
        let mut state = 0;
        assert_eq!(indicator_toggle_action(&mut state, Some(0), 3), "blinker_warn_toggle");
        assert_eq!(state, 3);
        assert_eq!(indicator_toggle_action(&mut state, Some(3), 3), "blinker_warn_toggle");
        assert_eq!(state, 0);
    }
}

#[cfg(test)]
mod steering_view_tests {
    use super::steering_view_yaw;

    #[test]
    fn follows_both_directions_without_snapping_or_overshooting() {
        let right = steering_view_yaw(0.0, 1.0, 0.016, true, 30.0, 0.25);
        assert!(right > 0.0 && right < 30.0);
        assert_eq!(steering_view_yaw(0.0, -1.0, 0.016, true, 30.0, 0.25), -right);
        let changed = steering_view_yaw(30.0, -1.0, 0.016, true, 30.0, 0.25);
        assert!(changed < 30.0 && changed > -30.0);
    }

    #[test]
    fn smoothing_is_frame_rate_independent_even_at_five_fps() {
        let mut reference: Option<f32> = None;
        for fps in [5, 30, 60, 144] {
            let mut yaw = 0.0;
            for _ in 0..fps { yaw = steering_view_yaw(yaw, 0.8, 1.0 / fps as f32, true, 40.0, 0.25); }
            if let Some(reference) = reference { assert!((yaw - reference).abs() < 0.0001); }
            reference = Some(yaw);
        }
    }

    #[test]
    fn configurable_angle_response_and_return_to_center() {
        let slow = steering_view_yaw(0.0, 1.0, 0.1, true, 45.0, 0.5);
        let fast = steering_view_yaw(0.0, 1.0, 0.1, true, 45.0, 0.1);
        assert!(fast > slow);
        assert_eq!(steering_view_yaw(0.0, 2.0, 0.1, true, 45.0, 0.1), fast);
        let centered = steering_view_yaw(30.0, 1.0, 0.1, false, 45.0, 0.25);
        assert!(centered > 0.0 && centered < 30.0);
        assert_eq!(steering_view_yaw(30.0, 0.0, 0.0, true, 45.0, 0.25), 30.0);
    }
}

/// The automated manual's choice (#713) from gear `cur` (of `top`), the engine speed as a
/// multiple of its idle, the pedals and the road speed.
pub(crate) fn auto_shift_gear(cur: i32, top: i32, n_over_idle: f32, throttle: f32, brake: f32, kmh: f32) -> i32 {
    if cur == 0 {
        return if throttle > 0.1 && brake < 0.05 && kmh < 3.0 && top >= 1 { 1 } else { 0 };
    }
    if cur < 1 {
        return cur;
    }
    // stopping: back to first, ready to pull away
    if cur > 1 && kmh < 5.0 {
        return 1;
    }
    let up = 2.2 + 1.2 * throttle.clamp(0.0, 1.0);
    if cur < top && throttle > 0.05 && n_over_idle > up {
        cur + 1
    } else if cur > 1 && n_over_idle < 1.35 {
        cur - 1
    } else {
        cur
    }
}

#[cfg(test)]
mod auto_shift_tests {
    use super::auto_shift_gear as g;

    #[test]
    fn the_automated_manual_shifts_by_the_engine_speed() {
        // pulling away: first gear in from neutral, never reverse
        assert_eq!(g(0, 5, 1.0, 0.5, 0.0, 0.0), 1);
        assert_eq!(g(0, 5, 1.0, 0.0, 0.0, 0.0), 0);
        assert_eq!(g(-1, 5, 3.0, 1.0, 0.0, 5.0), -1);
        // up early with a light foot, late with a heavy one
        assert_eq!(g(2, 5, 2.8, 0.3, 0.0, 30.0), 3);
        assert_eq!(g(2, 5, 2.8, 1.0, 0.0, 30.0), 2);
        assert_eq!(g(2, 5, 3.7, 1.0, 0.0, 30.0), 3);
        // never past the top gear, down near the idle, not below first
        assert_eq!(g(5, 5, 4.0, 1.0, 0.0, 90.0), 5);
        assert_eq!(g(3, 5, 1.2, 0.0, 0.5, 20.0), 2);
        assert_eq!(g(1, 5, 1.0, 0.0, 1.0, 2.0), 1);
        assert_eq!(g(4, 5, 1.0, 0.0, 1.0, 3.0), 1);
    }
}

#[cfg(test)]
mod door_action_tests {
    use super::door_action;

    /// The door actions that work on every bus (#916).
    #[test]
    fn door_actions_name_a_door_or_all_of_them() {
        assert_eq!(door_action("door_1"), Some(1));
        assert_eq!(door_action("Door_3"), Some(3));
        assert_eq!(door_action("doors_all"), Some(0));
        assert_eq!(door_action("door_0"), None);
        assert_eq!(door_action("bus_doorfront0"), None);
        assert_eq!(door_action("door_x"), None);
    }
}
