//! The dedicated server (`omsi --server server.cfg`): a session host that is always on, like
//! a Minecraft server - no window, no sound, no graphics card (the renderer runs on wgpu's
//! no-op device: the world, the traffic, the timetable and the people are simulated as the
//! host's game simulates them, and nothing is drawn). Players reach it at `host:port` (UDP)
//! or at its web address over a WebSocket (`omsi_net::ws`), which is what a free Cloudflare
//! tunnel carries: `tunnel = 1` starts one and prints the `https://….trycloudflare.com`
//! address to add in the launcher's Multiplayer → Servers.
//!
//! The folder of `server.cfg` is the server's: `server-icon.png` beside it is its icon (a
//! 64x64 PNG, like Minecraft's), `server.log` its log. A missing `server.cfg` is written with
//! the defaults and a comment for every key.

use super::*;
use std::path::Path;

/// What `server.cfg` says (see `DEFAULT_CFG`).
#[derive(Debug, Clone)]
pub(crate) struct ServerCfg {
    pub name: String,
    pub motd: String,
    pub map: String,
    pub date: Option<String>,
    pub time: String,
    pub weather: Option<String>,
    pub traffic: usize,
    pub timetable: bool,
    pub passengers: bool,
    pub port: u16,
    pub web_port: u16,
    pub max_players: usize,
    pub tunnel: bool,
    pub radius: i32,
    pub icon: Vec<u8>,
    /// Players who say `/admin <password>` in the chat administer the server (empty: nobody).
    pub admin_password: String,
    /// How fast the server's clock runs (1 real time).
    pub time_speed: f64,
    /// The clock follows the server machine's real date and time (the speed and the
    /// administration's clock are ignored then).
    pub real_time: bool,
    /// The weather follows an airport's METAR report, downloaded here; the players are told its
    /// values and need no sync of their own.
    pub metar_sync: bool,
    /// The airport of that report (ICAO; empty: the one nearest the map).
    pub metar_station: String,
    /// Only these buses may be driven on the server (vehicle files, empty: every bus the
    /// server has installed).
    pub vehicles: Vec<String>,
    /// `GET /players` on the web port tells who drives what and where (for a web map).
    pub share_positions: bool,
    /// The GreenTeaSpeak server and channel the players talk in (`voice`).
    pub voice: Option<crate::voice::VoiceServer>,
}

pub(crate) const DEFAULT_CFG: &str = "\
# openOMSI dedicated server
# (key = value; lines starting with # are comments)

# shown in the players' server list and when they join
name = openOMSI server
motd = Welcome! Drive safely.

# the map (relative to the OMSI 2 folder), the start date (YYYY-MM-DD, empty: today),
# the time of day and the weather (a .owt of the OMSI 2 folder, empty: the map's default,
# cycle: one after another through the day, as the month allows)
map = maps/Berlin-Spandau/global.cfg
date =
time = 08:00
weather =

# the shared world: random traffic (cars), timetable buses, waiting passengers
traffic = 30
timetable = 1
passengers = 1

# UDP port of the session, TCP port of the web gateway (status, icon, WebSocket players)
port = 27015
web_port = 27025
max_players = 16

# start a free Cloudflare quick tunnel (needs cloudflared) and print its https address
tunnel = 1

# how many tiles round the map's first entry point are kept loaded (0: all of them)
radius = 0

# players who say \"/admin <password>\" in the chat get the Administration menu (Esc):
# send players away, bring them, the clock, its speed, the weather (empty: nobody)
admin_password =

# how fast the clock runs (1 = real time, 2 = twice as fast, up to 30)
time_speed = 1

# the clock follows this machine's real date and time (1 = on; the time and date above are
# then only for the very first moment, and time_speed and the admin's clock are ignored)
real_time = 0

# the weather follows the real METAR report of an airport (1 = on; the weather above is then
# only for the first moment, and the admins cannot change it). The server downloads the report
# every ten minutes and tells the players its values, so they need no METAR sync of their own.
# metar_station is the airport's ICAO code, e.g. EDDB (empty: the one nearest the map)
metar_sync = 0
metar_station =

# the buses players may drive, separated by ; (vehicle files such as
# Vehicles/MAN_SD200/MAN_SD77.bus; empty: every bus installed on the server). The players'
# vehicle menu offers only these, and a player who drives another bus is sent away
vehicles =

# tell anyone who asks the web port (GET /players) the players' names, buses, lines and
# positions - for a live map of the server on a website; tell your players when it is on
share_positions = 0

# voice chat: the players hear each other where they stand, through GreenTeaSpeak and its
# openOMSI plugin (as SaltyChat does for FiveM). voice_server_uid is the voice server's unique
# id (its info panel; needed: without it there is no voice chat), voice_channel the in-game
# channel's id or name (empty: no voice chat), voice_range how far a player is heard (m).
# The channel and its password are sent to every player who joins
voice_server_uid =
voice_channel =
voice_channel_password =
voice_range = 20
";

impl ServerCfg {
    pub(crate) fn load(path: &Path) -> Result<ServerCfg> {
        if !path.is_file() {
            std::fs::write(path, DEFAULT_CFG).with_context(|| format!("writing {}", path.display()))?;
            log::info!("server: {} did not exist; written with the defaults", path.display());
        }
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut kv: std::collections::HashMap<String, String> = Default::default();
        for line in text.lines() {
            let l = line.trim();
            if l.is_empty() || l.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = l.split_once('=') {
                kv.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }
        let get = |k: &str, d: &str| kv.get(k).cloned().filter(|v| !v.is_empty()).unwrap_or_else(|| d.to_string());
        let flag = |k: &str, d: bool| kv.get(k).map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")).unwrap_or(d);
        let num = |k: &str, d: i64| kv.get(k).and_then(|v| v.parse::<i64>().ok()).unwrap_or(d);
        let dir = path.parent().unwrap_or(Path::new("."));
        let icon = std::fs::read(dir.join("server-icon.png")).ok().filter(|b| b.starts_with(b"\x89PNG") && b.len() < 256 * 1024).unwrap_or_default();
        Ok(ServerCfg {
            name: get("name", "openOMSI server"),
            motd: get("motd", ""),
            map: get("map", "maps/Berlin-Spandau/global.cfg"),
            date: kv.get("date").cloned().filter(|v| !v.is_empty()),
            time: get("time", "08:00"),
            weather: kv.get("weather").cloned().filter(|v| !v.is_empty()),
            traffic: num("traffic", 30).clamp(0, 200) as usize,
            timetable: flag("timetable", true),
            passengers: flag("passengers", true),
            port: num("port", 27015).clamp(1, 65535) as u16,
            web_port: num("web_port", 27025).clamp(1, 65535) as u16,
            max_players: num("max_players", 16).clamp(1, 64) as usize,
            tunnel: flag("tunnel", true),
            radius: num("radius", 0) as i32,
            icon,
            admin_password: kv.get("admin_password").cloned().unwrap_or_default(),
            time_speed: kv.get("time_speed").and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite()).unwrap_or(1.0).clamp(1.0, 30.0),
            real_time: flag("real_time", false),
            metar_sync: flag("metar_sync", false),
            metar_station: kv.get("metar_station").map(|v| v.chars().filter(|c| c.is_ascii_alphabetic()).take(4).collect::<String>().to_ascii_uppercase()).unwrap_or_default(),
            vehicles: kv.get("vehicles").map(|v| v.split(';').map(|x| x.trim().replace('\\', "/")).filter(|x| !x.is_empty()).collect()).unwrap_or_default(),
            share_positions: flag("share_positions", false),
            voice: crate::voice::VoiceServer::from_kv(|k| kv.get(k).cloned()),
        })
    }
}

/// The server's status as the web gateway tells it (see `omsi_net::ws::ServerInfo`).
pub(crate) fn info_of(cfg: &ServerCfg) -> omsi_net::ws::ServerInfo {
    omsi_net::ws::ServerInfo {
        name: cfg.name.clone(),
        motd: cfg.motd.clone(),
        map: cfg.map.clone(),
        players: 0,
        max_players: cfg.max_players,
        version: format!("{} ({})", env!("CARGO_PKG_VERSION"), BUILD),
        icon: cfg.icon.clone(),
        time: cfg.time.clone(),
        weather: cfg.weather.clone().unwrap_or_default(),
        password: false,
        vehicles: cfg.vehicles.clone(),
        reached_at: String::new(),
        players_public: cfg.share_positions,
        player_list: Vec::new(),
        local_admin_password: cfg.admin_password.clone(),
        ..Default::default()
    }
}

/// Set up the arguments for a server run from `server.cfg` (the rest is the offscreen
/// host loop, see `offscreen::run_offscreen`).
pub(crate) fn prepare(args: &mut Args, path: &Path) -> Result<ServerCfg> {
    let cfg = ServerCfg::load(path)?;
    SERVER_MODE.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = SERVER_ADMIN.set((cfg.admin_password.clone(), if cfg.real_time { 1.0 } else { cfg.time_speed }));
    crate::real_time::set_server_real(cfg.real_time);
    let _ = SERVER_VEHICLES.set(cfg.vehicles.clone());
    let _ = SERVER_VOICE.set(cfg.voice.clone());
    args.map = cfg.map.clone();
    args.time = cfg.time.clone();
    if let Some(d) = &cfg.date {
        args.date = Some(d.clone());
    }
    if cfg.real_time {
        crate::real_time::start_at_now(args);
    }
    args.weather = cfg.weather.clone();
    // the METAR sync: the report's weather from the start (the host loop downloads it again)
    let station = cfg.metar_sync.then(|| if cfg.metar_station.is_empty() { crate::launcher::drive::nearest_airport(&args.root.to_string_lossy(), &args.map) } else { cfg.metar_station.clone() });
    if let Some(icao) = station.as_ref() {
        match crate::weather_setup::try_metar(icao).and_then(|w| crate::weather_setup::report_wire(&w)) {
            Some(wire) => args.weather = Some(wire),
            None => log::warn!("server: no METAR report for {icao} yet; trying again soon"),
        }
        log::info!("server: the weather follows the METAR report of {icao}");
    }
    let _ = SERVER_METAR.set(station);
    args.traffic = cfg.traffic;
    args.schedule = cfg.timetable;
    args.passengers = cfg.passengers;
    args.lan_host = Some(cfg.port);
    args.lan_join = None;
    args.lan_name = cfg.name.clone();
    args.bus = None;
    args.radius = Some(if cfg.radius <= 0 { 99 } else { cfg.radius });
    args.size = "64x36".into();
    if args.offscreen.is_none() {
        args.offscreen = Some(std::env::temp_dir().join("omsi-server-unused.png"));
    }
    log::info!("server '{}': map {}, {} at {}, traffic {}, timetable {}, passengers {}, UDP {} / web {}, at most {} players", cfg.name, cfg.map, cfg.date.as_deref().unwrap_or("today"), cfg.time, cfg.traffic, cfg.timetable, cfg.passengers, cfg.port, cfg.web_port, cfg.max_players);
    Ok(cfg)
}

/// The airport whose METAR report a dedicated server's weather follows (None: no METAR sync).
pub(crate) static SERVER_METAR: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// The buses a dedicated server allows (`vehicles`; empty: every bus it has).
pub(crate) static SERVER_VEHICLES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// Whether a `vehicles` list allows the bus `file` (an empty list: every bus). A player's
/// game may name it with a folder before it (`D:/OMSI 2/Vehicles/…/….bus`).
pub(crate) fn allows(list: &[String], file: &str) -> bool {
    let norm = |s: &str| s.trim().replace('\\', "/").to_ascii_lowercase();
    let f = norm(file);
    list.is_empty() || list.iter().map(|v| norm(v)).any(|v| f == v || f.ends_with(&format!("/{v}")))
}

/// A server's `vehicles` list holds: a player who drives another bus (put down in the game's
/// own vehicle menu, which a game before this one did not limit to the list, #1183) is sent
/// away and told which buses the server has. It may join again with one of them.
pub(crate) fn enforce_vehicles(lan: &mut omsi_net::LanSession) {
    let Some(list) = SERVER_VEHICLES.get().filter(|l| !l.is_empty()) else { return };
    let out: Vec<(u32, String)> = lan.peers().filter(|p| p.has_info && !p.pose.bus.is_empty() && !allows(list, &p.pose.bus)).map(|p| (p.pose.id, p.pose.bus.clone())).collect();
    for (id, bus) in out {
        log::warn!("server: player {id} drives {bus}, which the vehicles list does not allow: sent away");
        let names: Vec<&str> = list.iter().map(|v| v.rsplit('/').next().unwrap_or(v)).collect();
        lan.kick(id, &format!("this server allows only these buses: {}", names.join(", ")), false);
    }
}

/// A dedicated server's voice server (`voice_*` of `server.cfg`).
pub(crate) static SERVER_VOICE: std::sync::OnceLock<Option<crate::voice::VoiceServer>> = std::sync::OnceLock::new();

/// A dedicated server's admin password and clock speed (for the host loop).
pub(crate) static SERVER_ADMIN: std::sync::OnceLock<(String, f64)> = std::sync::OnceLock::new();

/// A dedicated server run: graphics without a device, the whole world by interest.
pub(crate) static SERVER_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Every second of a server run: what the status page says (players, time, weather) and
/// who is where (`GET /players`, when `share_positions` is on).
pub(crate) fn tick_status(lan: &omsi_net::LanSession, time: f64, weather: &str) {
    let players = lan.peers().filter(|p| p.has_info).count();
    // (the admin's clock shift may take the time below 0 or past midnight: 23:08 had come
    // out as "00:-52")
    crate::lan::update_server_info(players, &crate::schedule::hhmm(time.rem_euclid(86400.0)), weather);
    let poses: Vec<&omsi_net::Pose> = lan.peers().filter(|p| p.has_info && p.has_pose).map(|p| &p.pose).collect();
    let list = poses.iter().filter_map(|q| player_info(q, |id| poses.iter().copied().find(|o| o.id == id))).collect();
    crate::lan::update_server_players(list);
}

/// A player for `GET /players`: on foot - a walker in its state - where it walks, or with the
/// bus it sits in (`Walker::aboard`; the walker's own point lags behind that bus); otherwise
/// with the bus it drives. The walker comes first: a player who got out keeps its vehicle, so
/// such a state carries `FLAG_VEHICLE` (the parked bus) and the walker. `None`: no place known.
pub(crate) fn player_info<'a>(q: &omsi_net::Pose, pose_of: impl Fn(u32) -> Option<&'a omsi_net::Pose>) -> Option<omsi_net::ws::PlayerInfo> {
    let driving = q.walker.is_none() && q.has_vehicle();
    let (x, y, heading, speed_kmh, aboard) = match q.walker {
        Some(w) => match w.aboard.and_then(|a| pose_of(a.owner)).filter(|b| b.has_vehicle()) {
            Some(b) => (b.x, b.y, b.heading, b.speed_kmh, Some(b.id)),
            // (a walker's speed is in m/s)
            None => (w.x, w.y, w.heading, w.speed * 3.6, None),
        },
        None if driving => (q.x, q.y, q.heading, q.speed_kmh, None),
        None => return None,
    };
    Some(omsi_net::ws::PlayerInfo {
        id: q.id,
        name: q.name.clone(),
        // (what a player on foot last drove is not what it drives)
        bus: if driving { q.bus.clone() } else { String::new() },
        line: if driving { q.line.clone() } else { String::new() },
        destination: if driving { q.destination.clone() } else { String::new() },
        tour: q.tour.clone(),
        x,
        y,
        heading,
        speed_kmh,
        on_foot: !driving,
        aboard,
        lat_lon: omsi_map::world_to_lat_lon(x, y),
    })
}

#[cfg(test)]
mod vehicles_tests {
    use super::allows;

    #[test]
    fn the_list_allows_its_buses_only() {
        let list = vec!["Vehicles/MAN_SD200/MAN_SD77.bus".to_string(), "Vehicles/MAN_SD202/MAN_D92.bus".to_string()];
        assert!(allows(&list, "Vehicles/MAN_SD200/MAN_SD77.bus"));
        // another case, backslashes, a folder before it
        assert!(allows(&list, "vehicles\\man_sd200\\MAN_SD77.bus"));
        assert!(allows(&list, "D:/OMSI 2/Vehicles/MAN_SD202/MAN_D92.bus"));
        // the issue's: another bus of the same folder, a mod bus
        assert!(!allows(&list, "Vehicles/MAN_SD200/MAN_SD83.bus"));
        assert!(!allows(&list, "Vehicles/Some_Mod/Bus.bus"));
        // (a file whose name only ends like a listed one)
        assert!(!allows(&list, "Vehicles/MAN_SD200/XMAN_SD77.bus"));
        // no list: every bus
        assert!(allows(&[], "Vehicles/Some_Mod/Bus.bus"));
    }
}

#[cfg(test)]
mod players_tests {
    use super::player_info;
    use omsi_net::{Aboard, Pose, Walker, FLAG_VEHICLE};

    fn bus(id: u32, x: f64, y: f64) -> Pose {
        Pose { id, name: format!("p{id}"), bus: "Vehicles/MAN_SD200/MAN_SD77.bus".into(), line: "37".into(), x, y, heading: 90.0, speed_kmh: 40.0, flags: FLAG_VEHICLE, ..Default::default() }
    }

    #[test]
    fn a_player_is_where_it_is() {
        let none = |_| None;
        // driving: the bus
        let d = player_info(&bus(2, 100.0, 200.0), none).unwrap();
        assert_eq!((d.x, d.y, d.speed_kmh, d.on_foot, d.aboard, d.line.as_str()), (100.0, 200.0, 40.0, false, None, "37"));
        // on foot: the walker, not the zeroed vehicle fields
        let mut w = Pose { id: 3, name: "p3".into(), bus: "Vehicles/MAN_SD200/MAN_SD77.bus".into(), ..Default::default() };
        w.walker = Some(Walker { x: 10.0, y: 20.0, heading: 45.0, speed: 1.5, ..Default::default() });
        let f = player_info(&w, none).unwrap();
        assert_eq!((f.x, f.y, f.heading, f.on_foot, f.bus.as_str()), (10.0, 20.0, 45.0, true, ""));
        assert!((f.speed_kmh - 5.4).abs() < 1e-4);
        // aboard player 2's bus: placed with it
        let driver = bus(2, 100.0, 200.0);
        w.walker = Some(Walker { x: 90.0, y: 190.0, aboard: Some(Aboard { owner: 2, ..Default::default() }), ..Default::default() });
        let a = player_info(&w, |id| (id == 2).then_some(&driver)).unwrap();
        assert_eq!((a.x, a.y, a.speed_kmh, a.on_foot, a.aboard), (100.0, 200.0, 40.0, true, Some(2)));
        // aboard a bus that is gone: its own point
        let g = player_info(&w, none).unwrap();
        assert_eq!((g.x, g.y, g.aboard), (90.0, 190.0, None));
        // got out of its own bus: the state keeps FLAG_VEHICLE and the parked bus's place, and
        // has the walker - the walker wins
        let mut out = bus(5, 100.0, 200.0);
        out.walker = Some(Walker { x: 104.0, y: 197.0, heading: 180.0, speed: 1.0, ..Default::default() });
        assert!(out.has_vehicle());
        let o = player_info(&out, none).unwrap();
        assert_eq!((o.x, o.y, o.heading, o.on_foot, o.aboard, o.bus.as_str(), o.line.as_str()), (104.0, 197.0, 180.0, true, None, "", ""));
        assert!((o.speed_kmh - 3.6).abs() < 1e-4);
        // and sitting in another player's bus after getting out of its own: with that bus
        out.walker = Some(Walker { x: 1.0, y: 1.0, aboard: Some(Aboard { owner: 2, ..Default::default() }), ..Default::default() });
        let oa = player_info(&out, |id| (id == 2).then_some(&driver)).unwrap();
        assert_eq!((oa.x, oa.y, oa.on_foot, oa.aboard), (100.0, 200.0, true, Some(2)));
        // neither a bus nor a walker: not listed
        assert!(player_info(&Pose { id: 4, ..Default::default() }, none).is_none());
    }
}
