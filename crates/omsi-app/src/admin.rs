//! Administration of a LAN session (Esc menu, Administration): the players (send away, send
//! away for the session, bring here, go to), the clock (an hour on or back, how fast it
//! runs), the weather, a notice to everybody.
//!
//! The host of a game started by code administers its own session from its game menu. A
//! dedicated server has no screen: a player who knows its `admin_password` says
//! `/admin <password>` in the chat, and the server's answer opens the same menu in that
//! player's game - every line of it is then sent to the server as `admin <action>` and done
//! there (`server_command`).
//!
//! The password never goes over the network: the player's game asks (`auth?`), the server
//! answers with a fresh challenge (`admin-challenge <hex>`), and the game sends
//! `auth <SHA-256(challenge, password)>`. Wrong answers lock the administration for a
//! while; a player's rights end when the player leaves.
//!
//! Actions (the text of a line, also what goes over the network): `kick <id>`,
//! `ban <id>`, `bring <id>`, `goto <id>`, `time <seconds>`, `speed <factor>`,
//! `weather next`, `say <text>`, `bringall`, `service <repair|refuel|wash> <id|all>`,
//! `unstick <id>`, `clock <seconds of the day>`, `traffic next`, `traffic clear`,
//! `weather cycle`, `weather set <Weather/file.owt>`.

use crate::App;
use omsi_net::{LanSession, Role};

/// The clock speeds offered.
pub(crate) const SPEEDS: [f64; 6] = [1.0, 2.0, 4.0, 8.0, 15.0, 30.0];

/// The lines of the administration menu: (label, action).
pub(crate) fn items(app: &App) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(lan) = app.lan.as_ref() else { return out };
    let mut peers: Vec<(u32, String)> = lan.peers().map(|p| (p.pose.id, if p.pose.name.is_empty() { format!("Player {}", p.pose.id) } else { p.pose.name.clone() })).collect();
    peers.sort();
    for (id, name) in &peers {
        if *id == lan.my_id {
            continue;
        }
        // (the labels in the interface's language around the player's name)
        let tr = |t: &str| omsi_ui::tr(t).into_owned();
        out.push((format!("{name}: {}", tr("go to")), format!("goto {id}")));
        out.push((format!("{name}: {}", tr("bring here")), format!("bring {id}")));
        out.push((format!("{name}: {}", tr("repair and refuel their bus")), format!("service repair {id}")));
        out.push((format!("{name}: {}", tr("put their bus back on its wheels")), format!("unstick {id}")));
        out.push((format!("{name}: {}", tr("send away")), format!("kick {id}")));
        out.push((format!("{name}: {}", tr("send away for the session")), format!("ban {id}")));
    }
    if peers.len() > 1 {
        out.push((omsi_ui::tr("Bring everybody here").into_owned(), "bringall".into()));
        out.push((omsi_ui::tr("Everybody: repair").into_owned(), "service repair all".into()));
        out.push((omsi_ui::tr("Everybody: refuel").into_owned(), "service refuel all".into()));
        out.push((omsi_ui::tr("Everybody: wash").into_owned(), "service wash all".into()));
    }
    for (label, secs) in [("Clock: 06:00 (morning)", 6 * 3600), ("Clock: 12:00 (noon)", 12 * 3600), ("Clock: 18:00 (evening)", 18 * 3600), ("Clock: 23:00 (night)", 23 * 3600)] {
        out.push((omsi_ui::tr(label).into_owned(), format!("clock {secs}")));
    }
    if let Some(t) = app.traffic.as_ref() {
        out.push((format!("{}: {} ({})", omsi_ui::tr("Traffic"), t.target, omsi_ui::tr("more / less")), "traffic next".into()));
    }
    out.push(("Clock +1 hour".into(), "time 3600".into()));
    out.push(("Clock -1 hour".into(), "time -3600".into()));
    let speed = lan.clock_speed;
    for s in SPEEDS {
        let mark = if (s - speed).abs() < 1e-6 { format!("  {}", omsi_ui::tr("(now)")) } else { String::new() };
        out.push((format!("{} x{s}{mark}", omsi_ui::tr("Time speed")), format!("speed {s}")));
    }
    out.push(("Next weather".into(), "weather next".into()));
    // the weather cycle, and each installed weather by name
    let cycling = app.weather_cycle.is_some();
    out.push((format!("{}: {}", omsi_ui::tr("Weather cycle"), if cycling { omsi_ui::tr("on") } else { omsi_ui::tr("off") }), "weather cycle".into()));
    for (file, w) in crate::weather_cycle::installed() {
        let now = app.args.weather.as_deref().is_some_and(|c| c.replace('\\', "/").eq_ignore_ascii_case(&file));
        let mark = if now { format!("  {}", omsi_ui::tr("(now)")) } else { String::new() };
        out.push((format!("{}: {}{mark}", omsi_ui::tr("Weather"), w.name), format!("weather set {file}")));
    }
    if app.traffic.is_some() {
        out.push((omsi_ui::tr("Clear the AI traffic (a jam)").into_owned(), "traffic clear".into()));
    }
    out.push(("Back".into(), "back".into()));
    out
}

/// A line of the menu chosen in this game.
pub(crate) fn run(app: &mut App, action: &str) {
    if action == "back" {
        return;
    }
    let Some(lan) = app.lan.as_mut() else { return };
    if lan.role == Role::Client {
        // (a server's admin: the server does it)
        lan.command(1, &format!("admin {action}"));
        app.service_msg = Some((format!("Sent to the server: {action}"), 3.0));
        return;
    }
    host_action(app, action, None);
}

/// Carry out an administration action as the host of a game (`by`: the admin who asked,
/// None: the host itself).
fn host_action(app: &mut App, action: &str, by: Option<u32>) {
    let (verb, arg) = action.split_once(' ').unwrap_or((action, ""));
    let id = arg.trim().parse::<u32>().ok();
    match verb {
        // a notification (`notify <id|all> <notice id> <seconds> <kind> <text>`): to the other
        // players through the session, and on the host's own screen for `all` or its own number
        "notify" => {
            let Some((who, rest)) = arg.trim().split_once(' ') else { return };
            let Some((nid, n)) = crate::ui::Notice::parse(rest) else { return };
            let mut here = false;
            if let Some(l) = app.lan.as_mut() {
                let (ids, local) = notice_targets(who, l.peers().map(|p| p.pose.id), l.my_id);
                for id in ids {
                    l.command(id, &format!("notify {}", rest.trim()));
                }
                here = local;
            }
            if here {
                log::info!("LAN: the server's notice {nid}: {}", n.text);
                crate::ui::push_notice(&mut app.notices, n);
            }
        }
        "kick" | "ban" => {
            if let (Some(l), Some(id)) = (app.lan.as_mut(), id) {
                l.kick(id, if verb == "ban" { "sent away for this session" } else { "sent away by the host" }, verb == "ban");
            }
        }
        "goto" => {
            let at = app.remotes.remotes.get(&id.unwrap_or(0)).map(|r| (r.vehicle().position, r.vehicle().heading));
            match (at, by) {
                (Some((pos, heading)), None) => teleport_beside(app, pos, heading),
                _ => app.service_msg = Some(("That player has no bus to go to".into(), 3.0)),
            }
        }
        "bring" => {
            // beside the host's bus (or the admin's), told to that player's game
            let here = match by {
                None => app.player.as_ref().map(|p| (p.vehicle.position, p.vehicle.heading)),
                Some(a) => app.remotes.remotes.get(&a).map(|r| (r.vehicle().position, r.vehicle().heading)),
            };
            if let (Some((pos, h)), Some(id), Some(l)) = (here, id, app.lan.as_mut()) {
                let (x, y) = beside(pos, h, 8.0);
                l.command(id, &format!("teleport {x:.2} {y:.2} {:.2} {h:.1}", pos.z));
            }
        }
        "time" => {
            if let Some(s) = finite(arg) {
                app.shift_clock(s.clamp(-86400.0, 86400.0));
            }
        }
        "speed" => {
            if app.real_time_locked() {
                app.service_msg = Some(("The time speed is fixed while the real-time sync is on".into(), 3.0));
            } else if let Some(s) = finite(arg) {
                let s = s.clamp(1.0, 30.0);
                if let Some(l) = app.lan.as_mut() {
                    l.clock_speed = s;
                }
                app.service_msg = Some((format!("Time speed x{s}"), 3.0));
            }
        }
        "weather" => match arg.trim().split_once(' ').map(|(a, b)| (a, b.trim())).unwrap_or((arg.trim(), "")) {
            ("cycle", _) => {
                if app.weather_cycle.take().is_none() {
                    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(7);
                    let mut c = crate::weather_cycle::Cycle::new(seed);
                    // (the first change soon, not in an hour)
                    c.next_in = 60.0;
                    app.weather_cycle = Some(c);
                }
                let on = app.weather_cycle.is_some();
                app.service_msg = Some((format!("Weather cycle {}", if on { "on" } else { "off" }), 3.0));
            }
            // (only an installed weather file: the name comes from the admin's game)
            ("set", file) if !file.contains("..") && file.to_ascii_lowercase().starts_with("weather/") && file.to_ascii_lowercase().ends_with(".owt") => {
                app.change_weather(Some(file.to_string()), true, 1.0);
            }
            _ => app.next_weather(),
        },
        "say" => {
            if let Some(l) = app.lan.as_mut() {
                let _ = l.say(arg);
            }
        }
        // everybody beside the host's bus (or the admin's), one behind the other
        "bringall" => {
            let here = match by {
                None => app.player.as_ref().map(|p| (p.vehicle.position, p.vehicle.heading)),
                Some(a) => app.remotes.remotes.get(&a).map(|r| (r.vehicle().position, r.vehicle().heading)),
            };
            if let (Some((pos, h)), Some(l)) = (here, app.lan.as_mut()) {
                let ids: Vec<u32> = l.peers().map(|p| p.pose.id).filter(|id| *id != l.my_id && Some(*id) != by).collect();
                for (k, id) in ids.iter().enumerate() {
                    let (x, y) = beside(pos, h, 5.0 * (k as f64 + 1.0));
                    l.command(*id, &format!("teleport {x:.2} {y:.2} {:.2} {h:.1}", pos.z));
                }
                app.service_msg = Some((format!("{} player(s) brought here", ids.len()), 3.0));
            }
        }
        // a service for one player's bus (or everybody's, the host's own too)
        "service" => {
            let (kind, who) = arg.split_once(' ').unwrap_or((arg, "all"));
            if !matches!(kind, "repair" | "refuel" | "wash") {
                return;
            }
            if let Some(l) = app.lan.as_mut() {
                let ids: Vec<u32> = if who == "all" { l.peers().map(|p| p.pose.id).filter(|id| *id != l.my_id).collect() } else { who.trim().parse::<u32>().ok().into_iter().collect() };
                for id in &ids {
                    l.command(*id, &format!("service {kind}"));
                }
            }
            if who == "all" {
                app.run_service(kind);
            }
        }
        "unstick" => {
            if let (Some(l), Some(id)) = (app.lan.as_mut(), who_id(arg)) {
                l.command(id, "unstick");
            }
        }
        "clock" => {
            if let Some(s) = finite(arg) {
                let d = (s.rem_euclid(86400.0) - app.clock.time + 43_200.0).rem_euclid(86_400.0) - 43_200.0;
                app.shift_clock(d);
            }
        }
        "traffic" if arg.trim() == "clear" => {
            // every AI vehicle off the road (the random traffic comes back by itself, the
            // timetable's buses with their next departures)
            if let (Some(t), Some(w), Some(r), Some(scene)) = (app.traffic.as_mut(), app.world.as_ref(), app.renderer.as_ref(), app.scene.as_mut()) {
                let ids: Vec<u64> = t.cars.iter().filter(|c| !c.is_bus()).map(|c| c.id).collect();
                for id in &ids {
                    t.remove_car(w, r, scene, *id);
                }
                app.service_msg = Some((format!("{} AI vehicles taken off the road", ids.len()), 3.0));
            }
        }
        "traffic" => {
            if let Some(t) = app.traffic.as_mut() {
                t.target = crate::game_lists::next_step(&crate::game_lists::TRAFFIC, t.target);
                app.args.traffic = t.target;
                app.service_msg = Some((format!("Traffic: {}", t.target), 3.0));
            }
        }
        _ => log::info!("admin: unknown action '{action}'"),
    }
}

/// A player's id (the argument of an action).
fn who_id(arg: &str) -> Option<u32> {
    arg.trim().parse::<u32>().ok()
}

/// A finite number (NaN passes `clamp` through and would stop everybody's clock).
fn finite(text: &str) -> Option<f64> {
    text.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

/// The password `/admin` was given, until the server's challenge comes.
static PENDING_PASSWORD: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Ask the server for its administration with `password` (see the module).
pub(crate) fn request(lan: &mut LanSession, password: &str) {
    *PENDING_PASSWORD.lock().unwrap_or_else(|e| e.into_inner()) = Some(password.to_string());
    lan.command(1, "auth?");
}

/// The answer to a challenge: SHA-256 of the challenge and the password, in hex.
fn response(challenge: &str, password: &str) -> String {
    use sha2::{Digest, Sha256};
    let h = Sha256::new().chain_update(b"omsi2rw-admin").chain_update(challenge.as_bytes()).chain_update([0u8]).chain_update(password.as_bytes()).finalize();
    h.iter().map(|b| format!("{b:02x}")).collect()
}

/// Equal strings, compared in time independent of where they differ.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A point `side` metres to the right of a vehicle at `pos` facing `heading`.
fn beside(pos: glam::DVec3, heading: f64, side: f64) -> (f64, f64) {
    let h = heading.to_radians();
    (pos.x + h.cos() * side, pos.y - h.sin() * side)
}

/// The own bus (or the walker) put beside another vehicle.
fn teleport_beside(app: &mut App, pos: glam::DVec3, heading: f64) {
    let (x, y) = beside(pos, heading, 8.0);
    teleport(app, glam::DVec3::new(x, y, pos.z), heading);
}

/// Our bus put at `at` facing `heading` (stopped), with the walker back at its wheel.
pub(crate) fn teleport(app: &mut App, at: glam::DVec3, heading: f64) {
    if app.on_foot.is_some() {
        app.back_to_bus();
    }
    // on the level at the height asked for (a car park under a building, a road under a
    // bridge), else the highest ground there (a place picked on the map, at no height)
    let ground = app.world.as_ref().and_then(|w| {
        let near = (at.z != 0.0)
            .then(|| crate::scene::drive_probe(&w.terrains, &w.surfaces, at.x, at.y, at.z + 1.5).below)
            .flatten()
            .filter(|b| (at.z - b).abs() < 3.0);
        near.or_else(|| w.walk_height(at.x, at.y))
    });
    let z = ground.unwrap_or(at.z);
    if let Some(p) = app.player.as_mut() {
        // (as a joining player's bus is moved off an occupied spawn: `lan::clear_spawn`)
        let origin = glam::DVec3::new(at.x, at.y, z);
        p.vehicle.position = origin;
        p.vehicle.heading = heading;
        if let Some(rb) = p.vehicle.rigid.as_mut() {
            rb.place(origin, heading);
        }
        for t in p.vehicle.trailers.iter_mut() {
            t.realign();
        }
        // (settled on the ground there; where its tiles are still to be read - a street far
        // off picked on the map - it waits at the street's height for them, as the frame
        // holds a bus with no ground under it, instead of dropping through first)
        if ground.is_some() {
            for _ in 0..3 {
                p.vehicle.update(1.0 / 30.0);
            }
        }
        log::info!("teleported to ({:.1}, {:.1}) heading {heading:.0}", at.x, at.y);
    }
}

/// A command from another game (the host's, or an admin's to the host).
pub(crate) fn command(app: &mut App, from: u32, text: &str) {
    let (verb, arg) = text.split_once(' ').unwrap_or((text, ""));
    match verb {
        // (host → us) put our bus there
        "teleport" if from == 1 => {
            let v: Vec<f64> = arg.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            if v.len() == 4 {
                teleport(app, glam::DVec3::new(v[0], v[1], v[2]), v[3]);
                app.service_msg = Some(("The host brought you to them".into(), 4.0));
            }
        }
        // (host → us) the host's object editor: a map object moved, turned or deleted…
        "objedit" if from == 1 => {
            let v: Vec<f64> = arg.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            if v.len() == 6 {
                let id = v[0] as i64;
                let e = crate::scene::ObjectEdit { moved: glam::DVec3::new(v[1], v[2], v[3]), turned: v[4], deleted: v[5] > 0.5 };
                let same = app.world.as_ref().and_then(|w| w.object_edits.lock().get(&id).copied()) == Some(e);
                if let (false, Some(w), Some(r), Some(scene)) = (same, app.world.clone(), app.renderer.as_ref(), app.scene.as_mut()) {
                    log::info!("LAN: the host's editor moved object {id} by ({:.2}, {:.2}, {:.2}), turned {:.1}, deleted {}", e.moved.x, e.moved.y, e.moved.z, e.turned, e.deleted);
                    w.apply_object_edit(r, scene, id, e);
                }
            }
        }
        // … or a new object (a copy), where it stands now
        "objadd" if from == 1 => {
            let mut it = arg.splitn(7, ' ');
            let nums: Vec<f64> = (0..6).filter_map(|_| it.next().and_then(|x| x.parse().ok())).collect();
            let rel = it.next().unwrap_or("").trim().to_string();
            if nums.len() == 6 && !rel.is_empty() {
                let id = nums[0] as i64;
                if let (Some(w), Some(r), Some(scene)) = (app.world.clone(), app.renderer.as_ref(), app.scene.as_mut()) {
                    if let Some(g) = app.remote_added.remove(&id) {
                        w.remove_helper_object(r, scene, g);
                    }
                    if nums[5] < 0.5 {
                        let path = app.args.root.join(&rel);
                        if let Some(g) = w.add_helper_object(r, scene, &path.to_string_lossy(), glam::DVec3::new(nums[1], nums[2], nums[3]), nums[4], &[]) {
                            app.remote_added.insert(id, g);
                        }
                    }
                }
            }
        }
        // (host → us) a service for our bus, or our bus back on its wheels
        "service" if from == 1 => {
            let kind = arg.trim();
            if matches!(kind, "repair" | "refuel" | "wash") {
                app.run_service(kind);
                app.service_msg = Some((format!("The host: {kind}"), 3.0));
            }
        }
        "unstick" if from == 1 => {
            if let Some(p) = app.player.as_ref() {
                let (at, heading) = (p.vehicle.position, p.vehicle.heading);
                teleport(app, at, heading);
                app.service_msg = Some(("The host put your bus back on its wheels".into(), 4.0));
            }
        }
        // (server → us) the password was right: the menu is ours
        // (server → us) prove the password without sending it
        "admin-challenge" if from == 1 => {
            let pw = PENDING_PASSWORD.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let (Some(pw), Some(l)) = (pw, app.lan.as_mut()) {
                l.command(1, &format!("auth {}", response(arg.trim(), &pw)));
            }
        }
        // (host → us) a duty given by the server's dispatch: `duty <line> <tour> <trip> <stop>`
        // - that trip of the tour (by its place among the tour's trips, as the tour list counts
        // them), from that stop (its number among the trip's stations; a stop the trip does not
        // serve: the next one it serves), as the tour list's start button does: the bus stays
        // where it is. The host hears `duty-ok <line> <tour> <trip> <stop>` or `duty-no <why>`.
        "duty" if from == 1 => {
            let reply = match parse_duty(arg) {
                None => "duty-no malformed".to_string(),
                Some((line, tour, trip, station)) => {
                    let stops = app.schedule.as_ref().map(|s| s.tour_trip_stops(&line, &tour, trip)).unwrap_or_default();
                    match stops.iter().position(|s| s.1 >= station).or(stops.len().checked_sub(1)) {
                        None => "duty-no unknown tour or trip".to_string(),
                        Some(chosen) => {
                            crate::game_lists::start_duty_at(app, &line, &tour, trip, chosen);
                            let ok = app.duty.as_ref().is_some_and(|d| d.line.eq_ignore_ascii_case(&line) && d.tour.eq_ignore_ascii_case(&tour));
                            log::info!("LAN: the server gave us line {line} tour {tour}, trip {trip} from stop {chosen}: {}", if ok { "taken" } else { "not taken" });
                            if ok {
                                format!("duty-ok {}", duty_arg(&line, &tour, trip, stops[chosen].1))
                            } else {
                                "duty-no no duty".to_string()
                            }
                        }
                    }
                }
            };
            if let Some(l) = app.lan.as_mut() {
                l.command(1, &reply);
            }
        }
        // (host → us) a notification over the navigator for a few seconds: `notify <id>
        // <seconds> <info|warn|alert> <text>`; the host hears that it was shown (`notify-seen
        // <id>`): a game that does not know `notify` stays silent, and the host can say it in
        // the chat instead
        "notify" if from == 1 => {
            if let Some((id, n)) = crate::ui::Notice::parse(arg) {
                log::info!("LAN: the server's notice {id}: {}", n.text);
                crate::ui::push_notice(&mut app.notices, n);
                if let Some(l) = app.lan.as_mut() {
                    l.command(1, &format!("notify-seen {id}"));
                }
            }
        }
        // (host → us) the server's dispatch takes the duty back: free drive, as the game menu's
        // "end the duty"; the host hears `duty-off-ok` (there was one) or `duty-off-none`
        "duty-off" if from == 1 => {
            let had = app.duty.take().map(|d| format!("{} {}", d.line, d.tour));
            log::info!("LAN: the server took our duty back ({})", had.as_deref().unwrap_or("we had none"));
            if had.is_some() {
                app.service_msg = Some(("The dispatch took the duty back: free drive".into(), 6.0));
            }
            if let Some(l) = app.lan.as_mut() {
                l.command(1, if had.is_some() { "duty-off-ok" } else { "duty-off-none" });
            }
        }
        "admin-locked" if from == 1 => app.service_msg = Some(("Too many wrong admin passwords: try again later".into(), 4.0)),
        "admin-ok" if from == 1 => {
            app.is_admin = true;
            app.service_msg = Some(("You administer this server now: Esc menu, Administration".into(), 6.0));
        }
        "admin-no" if from == 1 => app.service_msg = Some(("Wrong admin password".into(), 4.0)),
        // (a game hosting by code sent a notice: the player's game showed it)
        "notify-seen" => log::info!("LAN: player {from} saw notice {}", arg.trim()),
        // (host by code: only the host administers its own game)
        _ => log::info!("LAN: command '{text}' from player {from} not taken"),
    }
}

/// The administration of a dedicated server (`omsi --server`): what it keeps.
#[derive(Default)]
pub(crate) struct ServerAdmin {
    pub password: String,
    pub admins: std::collections::HashSet<u32>,
    /// The clock moved by an admin (s).
    pub shift: f64,
    /// An admin asked for the next weather.
    pub next_weather: bool,
    /// An admin set the clock to this time of day (s).
    pub set_clock: Option<f64>,
    /// An admin chose this weather (`Weather/….owt`, checked against the installed ones by
    /// the host loop).
    pub set_weather: Option<String>,
    /// The challenge each asking player was given (used once).
    challenges: std::collections::HashMap<u32, String>,
    /// When wrong answers came lately (the lock counts them, whoever sent them: a player
    /// who reconnects is somebody new).
    failures: Vec<std::time::Instant>,
}

/// Wrong answers within `LOCK_WINDOW` that lock the administration for everybody.
const LOCK_AFTER: usize = 5;
const LOCK_WINDOW: std::time::Duration = std::time::Duration::from_secs(120);

impl ServerAdmin {
    /// Forget the rights and challenges of players no longer in the session.
    pub fn prune(&mut self, lan: &LanSession) {
        let here: std::collections::HashSet<u32> = lan.peers().map(|p| p.pose.id).collect();
        self.admins.retain(|id| here.contains(id));
        self.challenges.retain(|id, _| here.contains(id));
    }

    fn locked(&mut self) -> bool {
        self.failures.retain(|t| t.elapsed() < LOCK_WINDOW);
        self.failures.len() >= LOCK_AFTER
    }
}

/// A weather file an admin may choose: a `Weather/….owt` path, nothing above it.
fn weather_file_ok(file: &str) -> bool {
    let f = file.replace('\\', "/").to_ascii_lowercase();
    f.starts_with("weather/") && f.ends_with(".owt") && !f.contains("..") && f.matches('/').count() == 1
}

/// Who the commands of the web gateway's `POST /admin` come from: no player has this id.
pub(crate) const LOCAL_ADMIN: u32 = u32::MAX;

/// A command a player sent the dedicated server.
pub(crate) fn server_command(lan: &mut LanSession, from: u32, text: &str, adm: &mut ServerAdmin, positions: &dyn Fn(u32) -> Option<(glam::DVec3, f64)>) {
    let (verb, arg) = text.split_once(' ').unwrap_or((text, ""));
    match verb {
        // a joining game asks which voice server the session talks on (`voice`)
        "voice?" => lan.command(from, &crate::voice::VoiceServer::command(crate::voice::hosted().as_ref())),
        "auth?" => {
            if adm.password.is_empty() {
                lan.command(from, "admin-no");
            } else if adm.locked() {
                lan.command(from, "admin-locked");
            } else {
                let c = format!("{:016x}{:016x}", omsi_net::random_session_id(), omsi_net::random_session_id());
                adm.challenges.insert(from, c.clone());
                lan.command(from, &format!("admin-challenge {c}"));
            }
        }
        "auth" => {
            let challenge = adm.challenges.remove(&from);
            if adm.locked() {
                lan.command(from, "admin-locked");
            } else if let (false, Some(c)) = (adm.password.is_empty(), challenge) {
                if same(arg.trim(), &response(&c, &adm.password)) {
                    adm.admins.insert(from);
                    log::info!("server: player {from} administers the server now");
                    lan.command(from, "admin-ok");
                } else {
                    adm.failures.push(std::time::Instant::now());
                    log::warn!("server: player {from} gave a wrong admin password");
                    lan.command(from, "admin-no");
                }
            } else {
                lan.command(from, "admin-no");
            }
        }
        "admin" if adm.admins.contains(&from) => {
            let (v, a) = arg.split_once(' ').unwrap_or((arg, ""));
            let id = a.trim().parse::<u32>().ok();
            log::info!("server: admin {from}: {arg}");
            match v {
                "kick" | "ban" => {
                    if let Some(id) = id {
                        lan.kick(id, if v == "ban" { "sent away for this session" } else { "sent away by an admin" }, v == "ban");
                    }
                }
                "bring" => {
                    if let (Some(id), Some((pos, h))) = (id, positions(from)) {
                        let (x, y) = beside(pos, h, 8.0);
                        lan.command(id, &format!("teleport {x:.2} {y:.2} {:.2} {h:.1}", pos.z));
                    }
                }
                "goto" => {
                    if let Some((pos, h)) = id.and_then(positions) {
                        let (x, y) = beside(pos, h, 8.0);
                        lan.command(from, &format!("teleport {x:.2} {y:.2} {:.2} {h:.1}", pos.z));
                    }
                }
                // (a server on the real time keeps its clock and its speed)
                "time" if !crate::real_time::server_real() => {
                    if let Some(s) = finite(a) {
                        adm.shift += s.clamp(-86400.0, 86400.0);
                    }
                }
                "speed" if !crate::real_time::server_real() => {
                    if let Some(s) = finite(a) {
                        lan.clock_speed = s.clamp(1.0, 30.0);
                    }
                }
                // the menu offers "weather next" and "weather set <file>" for each installed
                // weather; a server took every one of them for "next"
                "weather" => match a.trim().split_once(' ').map(|(k, f)| (k, f.trim())) {
                    Some(("set", file)) if weather_file_ok(file) => adm.set_weather = Some(file.replace('\\', "/")),
                    _ => adm.next_weather = true,
                },
                "say" => {
                    let _ = lan.say(a);
                }
                // a word for one player only: `tell <id> <text>`, a chat line from "Admin
                // (private)" that the others do not get
                "tell" => {
                    if let Some((who, msg)) = a.trim().split_once(' ') {
                        if let Ok(id) = who.parse::<u32>() {
                            if let Err(e) = lan.say_to(id, "Admin (private)", msg.trim()) {
                                log::info!("server: tell {id}: {e}");
                            }
                        }
                    }
                }
                // a duty for one player: `duty <id> <line> <tour> <trip> <stop>` (see `command`)
                "duty" => {
                    if let Some((who, rest)) = a.trim().split_once(' ') {
                        if let (Ok(id), Some(_)) = (who.parse::<u32>(), parse_duty(rest)) {
                            lan.command(id, &format!("duty {}", rest.trim()));
                        }
                    }
                }
                // a notification on one player's screen, or everybody's: `notify <id|all> <notice
                // id> <seconds> <info|warn|alert> <text>` (a game that shows it answers
                // `notify-seen <notice id>`)
                // (a dedicated server has no screen: its own number is nobody's)
                "notify" => {
                    if let Some((who, rest)) = a.trim().split_once(' ') {
                        if crate::ui::Notice::parse(rest).is_some() {
                            let (ids, _) = notice_targets(who, lan.peers().map(|p| p.pose.id), lan.my_id);
                            for id in ids {
                                lan.command(id, &format!("notify {}", rest.trim()));
                            }
                        }
                    }
                }
                // the duty taken back from a player: `duty-off <id>` (see `command`)
                "duty-off" => {
                    if let Some(id) = id {
                        lan.command(id, "duty-off");
                    }
                }
                "bringall" => {
                    if let Some((pos, h)) = positions(from) {
                        let ids: Vec<u32> = lan.peers().map(|p| p.pose.id).filter(|id| *id != from && *id != lan.my_id).collect();
                        for (k, id) in ids.iter().enumerate() {
                            let (x, y) = beside(pos, h, 5.0 * (k as f64 + 1.0));
                            lan.command(*id, &format!("teleport {x:.2} {y:.2} {:.2} {h:.1}", pos.z));
                        }
                    }
                }
                "service" => {
                    let (kind, who) = a.split_once(' ').unwrap_or((a, "all"));
                    if matches!(kind, "repair" | "refuel" | "wash") {
                        let ids: Vec<u32> = if who == "all" { lan.peers().map(|p| p.pose.id).filter(|id| *id != lan.my_id).collect() } else { who.trim().parse::<u32>().ok().into_iter().collect() };
                        for id in ids {
                            lan.command(id, &format!("service {kind}"));
                        }
                    }
                }
                "unstick" => {
                    if let Some(id) = id {
                        lan.command(id, "unstick");
                    }
                }
                "clock" if !crate::real_time::server_real() => {
                    // (the server's clock is the session's: moved by the difference)
                    if let Some(s) = finite(a) {
                        adm.set_clock = Some(s.rem_euclid(86400.0));
                    }
                }
                _ => {}
            }
        }
        // a player's game took the duty it was given (`duty`), or could not: said for the tool
        // that gave it
        "duty-off-ok" => log::info!("server: player {from} left the duty"),
        "duty-off-none" => log::info!("server: player {from} had no duty to leave"),
        "duty-ok" => log::info!("server: player {from} took duty {}", arg.trim()),
        "duty-no" => log::info!("server: player {from} could not take the duty: {}", arg.trim()),
        // a player's game showed a notification (`notify`): said for the tool that sent it
        "notify-seen" => log::info!("server: player {from} saw notice {}", arg.trim()),
        _ => log::info!("server: command '{text}' from player {from} not taken"),
    }
}

/// The bus fallen through the world (a hole in the ground, a tile that was not there yet,
/// a mod's road without a surface) fell for ever. Where it last stood on the ground is
/// kept every second; a bus more than 8 m under the ground there is - or 40 m under where
/// it last stood, where there is no ground - is put back there.
pub(crate) fn guard_fall(app: &mut App, dt: f32) {
    let Some(p) = app.player.as_ref() else { return };
    let at = p.vehicle.position;
    // the ground under the bus (the face at or below it: on a car park's lower level the
    // building's roof is not its ground - measured from the roof, a bus driving under it
    // had fallen through the world and was put up there), and the highest there is
    let (under, ground) = match app.world.as_ref() {
        Some(w) => (crate::scene::drive_probe(&w.terrains, &w.surfaces, at.x, at.y, at.z + 1.5).below, w.walk_height(at.x, at.y)),
        None => (None, None),
    };
    app.safe_age += dt;
    let fallen = match (under, ground) {
        (Some(_), _) => false,
        (None, Some(g)) => at.z < g - 8.0,
        (None, None) => app.safe_pose.map(|s| at.z < s.0.z - 40.0).unwrap_or(false),
    };
    if fallen {
        if let Some((pos, heading)) = app.safe_pose {
            log::warn!("the bus fell through the world at ({:.1}, {:.1}, {:.1}): put back at ({:.1}, {:.1})", at.x, at.y, at.z, pos.x, pos.y);
            teleport(app, pos, heading);
            app.service_msg = Some(("The bus fell through the ground: it was put back where it last stood".into(), 5.0));
        }
        return;
    }
    if app.safe_age >= 1.0 {
        if let Some(g) = under.filter(|g| (at.z - g).abs() < 2.5) {
            app.safe_age = 0.0;
            app.safe_pose = Some((glam::DVec3::new(at.x, at.y, g), p.vehicle.heading));
        }
    }
}

/// `duty`'s argument: `<line> <tour> <trip> <stop>`, a space in the line or tour name written
/// `%20` (and `%` as `%25`).
pub(crate) fn duty_arg(line: &str, tour: &str, trip: usize, stop: usize) -> String {
    let esc = |s: &str| s.trim().replace('%', "%25").replace(' ', "%20");
    format!("{} {} {trip} {stop}", esc(line), esc(tour))
}

/// `duty_arg` read back: (line, tour, trip, stop).
pub(crate) fn parse_duty(arg: &str) -> Option<(String, String, usize, usize)> {
    let unesc = |s: &str| s.replace("%20", " ").replace("%25", "%");
    let v: Vec<&str> = arg.split_whitespace().collect();
    let [line, tour, trip, stop] = v[..] else { return None };
    Some((unesc(line), unesc(tour), trip.parse().ok()?, stop.parse().ok()?))
}

#[cfg(test)]
mod duty_tests {
    use super::{duty_arg, parse_duty};

    #[test]
    fn a_duty_goes_to_the_game_and_back() {
        assert_eq!(duty_arg("15", "5", 2, 7), "15 5 2 7");
        assert_eq!(parse_duty("15 5 2 7"), Some(("15".into(), "5".into(), 2, 7)));
        // names with spaces and percent signs
        let a = duty_arg("KI-Zug", "ZOB RB 100%", 0, 0);
        assert_eq!(a, "KI-Zug ZOB%20RB%20100%25 0 0");
        assert_eq!(parse_duty(&a), Some(("KI-Zug".into(), "ZOB RB 100%".into(), 0, 0)));
        // missing or extra parts, not numbers: not a duty
        for bad in ["", "15", "15 5", "15 5 2", "15 5 2 7 9", "15 5 x 7", "15 5 2 -1"] {
            assert_eq!(parse_duty(bad), None, "{bad}");
        }
    }
}

/// Who a `notify` is for: `all`, every other player and the host's own screen; a player's
/// number, that player - or the host's own screen when it is the host's number (`peers` has
/// only the others, and `LanSession::command` sends nothing to oneself).
fn notice_targets(who: &str, others: impl Iterator<Item = u32>, my_id: u32) -> (Vec<u32>, bool) {
    if who == "all" {
        return (others.filter(|id| *id != my_id).collect(), true);
    }
    match who.trim().parse::<u32>() {
        Ok(id) if id == my_id => (Vec::new(), true),
        Ok(id) => (vec![id], false),
        Err(_) => (Vec::new(), false),
    }
}

#[cfg(test)]
mod notice_target_tests {
    use super::notice_targets;

    #[test]
    fn all_is_every_player_and_the_host_itself() {
        // the host is player 1, the others 2 and 5
        assert_eq!(notice_targets("all", [2, 5].into_iter(), 1), (vec![2, 5], true));
        assert_eq!(notice_targets("all", [].into_iter(), 1), (vec![], true));
        // one player; the host's own number: its own screen, nothing sent
        assert_eq!(notice_targets("5", [2, 5].into_iter(), 1), (vec![5], false));
        assert_eq!(notice_targets("1", [2, 5].into_iter(), 1), (vec![], true));
        // not a number: nobody
        assert_eq!(notice_targets("x", [2, 5].into_iter(), 1), (vec![], false));
    }
}

#[cfg(test)]
mod weather_file_tests {
    use super::weather_file_ok;

    #[test]
    fn only_a_weather_file() {
        assert!(weather_file_ok("Weather/#CAVOK.owt"));
        assert!(weather_file_ok("weather\\Bodennebel.OWT"));
        assert!(!weather_file_ok("Weather/../server.cfg"));
        assert!(!weather_file_ok("Weather/sub/x.owt"));
        assert!(!weather_file_ok("maps/x.owt"));
        assert!(!weather_file_ok("Weather/x.cfg"));
    }
}
