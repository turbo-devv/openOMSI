//! The session's datagrams over a WebSocket, for the ways in that carry nothing but HTTP: a
//! Cloudflare tunnel (`cloudflared tunnel --url …` gives a free `https://….trycloudflare.com`
//! address and passes WebSockets, never UDP), a reverse proxy in front of a server.
//!
//! Nothing of the game protocol changes: each side keeps talking UDP to a socket on its own
//! machine and the bridge carries every datagram as one binary WebSocket message.
//!
//! * [`WsGateway`] (the host's or the server's side) listens for HTTP on a TCP port. A
//!   WebSocket there gets a UDP socket of its own on 127.0.0.1, so the session sees every
//!   player coming in this way as an address of its own. The same port answers
//!   `GET /status` (a small JSON object about the server, for the launcher's list),
//!   `GET /icon.png` and, when the server shares them (`share_positions`), `GET /players`:
//!   who drives what and where, for a web map of the server. A dedicated server with an
//!   admin password also takes `POST /admin`
//!   from the machine it runs on (see [`local_admin`]): the administration a tool beside the
//!   server uses, without joining the session.
//! * [`WsClient`] (a joining game) connects to `wss://…/ws`, binds a UDP socket on
//!   127.0.0.1 and gives its address to `LanSession::join`; whatever the game sends there
//!   goes over the WebSocket and back.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tungstenite::{Message, WebSocket};

/// What a server tells about itself (`GET /status`, and the launcher's list).
#[derive(Debug, Clone, Default)]
pub struct ServerInfo {
    pub name: String,
    pub motd: String,
    pub map: String,
    pub players: usize,
    pub max_players: usize,
    pub version: String,
    /// A PNG (64x64 like a Minecraft server's), empty for none.
    pub icon: Vec<u8>,
    /// The time of day and weather the world has now (for the list).
    pub time: String,
    pub weather: String,
    pub password: bool,
    /// The buses that may be driven there (vehicle files, `Vehicles/…/….bus`): what the
    /// host has installed, or a server's own list. Empty: not said (an older game).
    pub vehicles: Vec<String>,
    /// Where it answered (`http(s)://…`), set by `query`: a server added by its bare
    /// address (`1.2.3.4`, `host:27025`) is joined there.
    pub reached_at: String,
    /// `GET /players` answers (the server shares its players' positions); otherwise 404.
    pub players_public: bool,
    /// The players now, for `GET /players`.
    pub player_list: Vec<PlayerInfo>,
    /// `POST /admin` from this machine with this password (empty: no such door).
    pub local_admin_password: String,
    /// The admin commands that came in that way, for the host loop to run.
    pub local_admin_queue: Vec<String>,
    /// When wrong passwords came lately (they lock the door for a while).
    pub local_admin_failures: Vec<Instant>,
    /// A dedicated server's shared world now (`"world"` in `GET /status`); none elsewhere.
    pub world: Option<WorldCounts>,
}

/// What a dedicated server's shared world holds: the AI cars on the roads (`cars`), its
/// timetable buses (`buses`), the cars put to sleep far from every player (`dormant`), the
/// parked cars (`parked`), the people walking (`walking`), waiting at a stop (`waiting`) or
/// in a bus (`aboard`), and the traffic density asked for (`traffic`, as `server.cfg`'s).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WorldCounts {
    pub cars: usize,
    pub buses: usize,
    pub dormant: usize,
    pub parked: usize,
    pub walking: usize,
    pub waiting: usize,
    pub aboard: usize,
    pub traffic: usize,
}

impl WorldCounts {
    pub fn to_json(&self) -> String {
        format!(
            "{{\"cars\":{},\"buses\":{},\"dormant\":{},\"parked\":{},\"walking\":{},\"waiting\":{},\"aboard\":{},\"traffic\":{}}}",
            self.cars, self.buses, self.dormant, self.parked, self.walking, self.waiting, self.aboard, self.traffic
        )
    }
}

/// A player as `GET /players` tells it: a web map of the server draws it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlayerInfo {
    pub id: u32,
    pub name: String,
    /// Vehicle file (`Vehicles/…/….bus`), empty for a player on foot.
    pub bus: String,
    pub line: String,
    pub destination: String,
    /// The timetable tour, `<line>/<tour>` (empty for none).
    pub tour: String,
    /// World metres (x east, y north) and heading (degrees, clockwise from north): the bus
    /// driven, or the player on foot, or the bus the player sits in.
    pub x: f64,
    pub y: f64,
    pub heading: f32,
    pub speed_kmh: f32,
    /// Not driving: walking, or aboard another player's bus (`aboard`: that player's id).
    pub on_foot: bool,
    pub aboard: Option<u32>,
    /// Where that is on the earth, on a `[worldcoordinates]` map.
    pub lat_lon: Option<(f64, f64)>,
}

impl PlayerInfo {
    pub fn to_json(&self) -> String {
        let num = |v: f64, digits: usize| if v.is_finite() { format!("{v:.digits$}") } else { "null".into() };
        let (lat, lon) = match self.lat_lon {
            Some((a, o)) => (num(a, 6), num(o, 6)),
            None => ("null".into(), "null".into()),
        };
        format!(
            "{{\"id\":{},\"name\":{},\"bus\":{},\"line\":{},\"destination\":{},\"tour\":{},\"x\":{},\"y\":{},\"heading\":{},\"speed_kmh\":{},\"on_foot\":{},\"aboard\":{},\"lat\":{},\"lon\":{}}}",
            self.id,
            json_str(&self.name),
            json_str(&self.bus),
            json_str(&self.line),
            json_str(&self.destination),
            json_str(&self.tour),
            num(self.x, 1),
            num(self.y, 1),
            num(self.heading as f64, 1),
            num(self.speed_kmh as f64, 1),
            self.on_foot,
            self.aboard.map(|a| a.to_string()).unwrap_or_else(|| "null".into()),
            lat,
            lon
        )
    }
}

/// `GET /players`: a JSON array of the players.
pub fn players_json(players: &[PlayerInfo]) -> String {
    format!("[{}]", players.iter().map(PlayerInfo::to_json).collect::<Vec<_>>().join(","))
}

impl ServerInfo {
    pub fn to_json(&self) -> String {
        format!(
            "{{\"name\":{},\"motd\":{},\"map\":{},\"players\":{},\"max_players\":{},\"version\":{},\"icon\":{},\"time\":{},\"weather\":{},\"password\":{},\"protocol\":{},\"vehicles\":{},\"world\":{}}}",
            json_str(&self.name),
            json_str(&self.motd),
            json_str(&self.map),
            self.players,
            self.max_players,
            json_str(&self.version),
            !self.icon.is_empty(),
            json_str(&self.time),
            json_str(&self.weather),
            self.password,
            crate::PROTOCOL,
            json_str(&self.vehicles.join(";")),
            self.world.map(|w| w.to_json()).unwrap_or_else(|| "null".into())
        )
    }

    /// Read what `to_json` wrote (the launcher asking a server).
    pub fn from_json(s: &str) -> Option<ServerInfo> {
        let text = |k: &str| json_value(s, k).map(|v| v.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(&v).to_string().replace("\\\"", "\"").replace("\\\\", "\\").replace("\\n", "\n"));
        let num = |k: &str| json_value(s, k).and_then(|v| v.trim().parse::<usize>().ok());
        Some(ServerInfo {
            name: text("name")?,
            motd: text("motd").unwrap_or_default(),
            map: text("map").unwrap_or_default(),
            players: num("players").unwrap_or(0),
            max_players: num("max_players").unwrap_or(0),
            version: text("version").unwrap_or_default(),
            icon: if json_value(s, "icon").map(|v| v.trim() == "true").unwrap_or(false) { vec![1] } else { Vec::new() },
            time: text("time").unwrap_or_default(),
            weather: text("weather").unwrap_or_default(),
            password: json_value(s, "password").map(|v| v.trim() == "true").unwrap_or(false),
            vehicles: text("vehicles").map(|v| v.split(';').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()).unwrap_or_default(),
            reached_at: String::new(),
            ..Default::default()
        })
    }
}

fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c if c.is_control() => {}
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// The raw value of `key` in a flat JSON object (a string with its quotes, or a number).
fn json_value(s: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":");
    let start = s.find(&pat)? + pat.len();
    let rest = s[start..].trim_start();
    if let Some(r) = rest.strip_prefix('"') {
        let mut out = String::from("\"");
        let mut esc = false;
        for c in r.chars() {
            if esc {
                out.push('\\');
                out.push(c);
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                out.push('"');
                return Some(out);
            } else {
                out.push(c);
            }
        }
        None
    } else {
        let end = rest.find([',', '}']).unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }
}

/// A WebSocket address for what a player typed: `https://x` → `wss://x/ws`, `http://x` →
/// `ws://x/ws`, `ws(s)://…` as it is. None for anything else (an address or a code).
pub fn ws_url(target: &str) -> Option<String> {
    let t = target.trim();
    let (scheme, rest) = if let Some(r) = t.strip_prefix("https://") {
        ("wss://", r)
    } else if let Some(r) = t.strip_prefix("http://") {
        ("ws://", r)
    } else if t.starts_with("wss://") || t.starts_with("ws://") {
        return Some(t.to_string());
    } else if t.ends_with(".trycloudflare.com") || t.contains(".trycloudflare.com/") {
        ("wss://", t)
    } else {
        return None;
    };
    let rest = rest.trim_end_matches('/');
    Some(if rest.ends_with("/ws") { format!("{scheme}{rest}") } else { format!("{scheme}{rest}/ws") })
}

/// The `https://` base of a server address (for `/status` and `/icon.png`).
pub fn http_base(target: &str) -> Option<String> {
    let u = ws_url(target)?;
    let u = u.strip_suffix("/ws").unwrap_or(&u).to_string();
    Some(u.replacen("wss://", "https://", 1).replacen("ws://", "http://", 1))
}

/// The web addresses a server may answer at, for whatever address a player gave: a web
/// address as it is; a bare `host` or `host:port` (an IP, a domain) at that port, at the
/// port ten above it (the game's port was given: a server's web port is its game port + 10
/// unless `web_port` says otherwise), at the servers' default 27025, and over https.
pub fn web_bases(target: &str) -> Vec<String> {
    if let Some(b) = http_base(target) {
        return vec![b];
    }
    let t = target.trim().trim_end_matches('/');
    let t = t.strip_suffix("/ws").unwrap_or(t);
    if t.is_empty() || t.contains(char::is_whitespace) {
        return Vec::new();
    }
    // (an IPv6 address is written in brackets with a port: [::1]:27025)
    let (host, port) = match t.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && (!h.contains(':') || h.ends_with(']')) => match p.parse::<u16>() {
            Ok(p) => (h.to_string(), Some(p)),
            Err(_) => (t.to_string(), None),
        },
        _ if t.contains(':') && !t.starts_with('[') => (format!("[{t}]"), None),
        _ => (t.to_string(), None),
    };
    let mut out = Vec::new();
    let mut add = |u: String| {
        if !out.contains(&u) {
            out.push(u);
        }
    };
    if let Some(p) = port {
        add(format!("http://{host}:{p}"));
        if p <= 65525 {
            add(format!("http://{host}:{}", p + 10));
        }
    }
    add(format!("http://{host}:27025"));
    if port.is_none() {
        add(format!("https://{host}"));
        add(format!("http://{host}"));
    }
    out
}

/// Ask a server (by its address as typed) about itself: its status and its icon.
pub fn query(target: &str, with_icon: bool) -> Result<ServerInfo, String> {
    let target = &crate::official::resolve_target(target)?;
    let bases = web_bases(target);
    if bases.is_empty() {
        return Err("no address given".into());
    }
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(6)).user_agent("openOMSI").build();
    let mut err = String::new();
    let mut found = None;
    for base in bases {
        match agent.get(&format!("{base}/status")).call().map_err(|e| e.to_string()).and_then(|r| r.into_string().map_err(|e| e.to_string())) {
            Ok(body) => match ServerInfo::from_json(&body) {
                Some(i) => {
                    found = Some((base, i));
                    break;
                }
                None => err = "the answer is not an openOMSI server's".into(),
            },
            Err(e) => {
                if err.is_empty() {
                    err = e;
                }
            }
        }
    }
    let (base, mut info) = found.ok_or(err)?;
    info.reached_at = base.clone();
    if with_icon && !info.icon.is_empty() {
        info.icon.clear();
        if let Ok(r) = agent.get(&format!("{base}/icon.png")).call() {
            let mut buf = Vec::new();
            if r.into_reader().take(512 * 1024).read_to_end(&mut buf).is_ok() && buf.starts_with(b"\x89PNG") {
                info.icon = buf;
            }
        }
    }
    Ok(info)
}

/// Connections the gateway serves at once (players, status requests, mod streams).
const MAX_CONNECTIONS: usize = 64;

/// The host's or server's side (see the module).
pub struct WsGateway {
    pub addr: SocketAddr,
    pub info: Arc<Mutex<ServerInfo>>,
    /// Players connected over WebSockets now.
    pub connected: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

impl Drop for WsGateway {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl WsGateway {
    /// Listen on `listen` (TCP) and carry WebSockets to the session at `target` (UDP).
    pub fn start(listen: SocketAddr, target: SocketAddr, info: ServerInfo) -> std::io::Result<WsGateway> {
        let listener = TcpListener::bind(listen)?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let info = Arc::new(Mutex::new(info));
        let connected = Arc::new(AtomicUsize::new(0));
        let (st, inf, conn) = (stop.clone(), info.clone(), connected.clone());
        std::thread::Builder::new().name("ws gateway".into()).spawn(move || {
            // connections served at once (each its own thread): more are closed at once
            let open = Arc::new(AtomicUsize::new(0));
            while !st.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, peer)) => {
                        if open.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
                            log::debug!("ws gateway: {peer}: {MAX_CONNECTIONS} connections open already; closed");
                            drop(stream);
                            continue;
                        }
                        open.fetch_add(1, Ordering::Relaxed);
                        let (st, inf, conn, held) = (st.clone(), inf.clone(), conn.clone(), open.clone());
                        let spawned = std::thread::Builder::new().name("ws player".into()).spawn(move || {
                            if let Err(e) = serve(stream, target, &inf, &st, &conn) {
                                log::debug!("ws gateway: {peer}: {e}");
                            }
                            held.fetch_sub(1, Ordering::Relaxed);
                        });
                        if spawned.is_err() {
                            open.fetch_sub(1, Ordering::Relaxed);
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(40)),
                    Err(e) => {
                        log::warn!("ws gateway: accept: {e}");
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            }
        })?;
        log::info!("ws gateway: WebSockets on {addr} carry the session at {target} (GET /status, /icon.png)");
        Ok(WsGateway { addr, info, connected, stop })
    }
}

/// Wrong local admin passwords within `ADMIN_LOCK_WINDOW` that close the door for a while.
const ADMIN_LOCK_AFTER: usize = 5;
const ADMIN_LOCK_WINDOW: Duration = Duration::from_secs(120);

/// The rest of a request's body, up to its `Content-Length` (4 KiB at most).
fn read_body(s: &mut TcpStream, request: &mut Vec<u8>) {
    let text = String::from_utf8_lossy(request).to_string();
    let Some(head_end) = text.find("\r\n\r\n") else { return };
    let want = header(&text[..head_end], "content-length").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0).min(4096);
    let mut buf = [0u8; 1024];
    while request.len() < head_end + 4 + want {
        match s.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => request.extend_from_slice(&buf[..n]),
        }
    }
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case(name)).map(|(_, v)| v.trim()))
}

/// Where a WebSocket comes from: its peer's address, or - from the loopback, as through a
/// reverse proxy (Caddy, nginx) or a tunnel on the same machine - the client's address that
/// proxy forwards (`X-Forwarded-For`'s first entry, `X-Real-IP`, `CF-Connecting-IP`).
fn client_addr(head: &str, peer: Option<SocketAddr>) -> String {
    let Some(peer) = peer else { return "?".into() };
    if peer.ip().is_loopback() {
        let forwarded = header(head, "cf-connecting-ip")
            .or_else(|| header(head, "x-forwarded-for").and_then(|v| v.split(',').next()))
            .or_else(|| header(head, "x-real-ip"))
            .map(str::trim)
            .and_then(|v| v.parse::<std::net::IpAddr>().ok());
        if let Some(ip) = forwarded {
            return ip.to_string();
        }
    }
    peer.ip().to_string()
}

/// Compare two secrets in a time that does not tell how much of them matched.
fn same_secret(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut d = (a.len() ^ b.len()) as u8 | (a.len() != b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        d |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    d == 0
}

/// `POST /admin`: admin commands (one a line, as the Administration menu sends them:
/// `clock 30600`, `weather next`, `say …`, `kick 3` …) for a dedicated server, from a tool on
/// the same machine - a web dispatch page, a script. Only from the loopback, only with the
/// server's admin password in `X-Admin-Password`; five wrong ones in two minutes close it
/// for a while. A reverse proxy on the same machine forwards from 127.0.0.1 too: it must
/// not pass `/admin` on.
pub fn local_admin(request: &[u8], peer: Option<SocketAddr>, info: &Mutex<ServerInfo>) -> (&'static str, String) {
    let text = String::from_utf8_lossy(request);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut i = info.lock().unwrap_or_else(|e| e.into_inner());
    if i.local_admin_password.is_empty() {
        return ("404 Not Found", "no admin password on this server".into());
    }
    if !peer.map(|p| p.ip().is_loopback()).unwrap_or(false) {
        return ("403 Forbidden", "only from this machine".into());
    }
    // A tunnel or a proxy on this machine connects from the loopback as well: the server's
    // own cloudflared tunnel (`tunnel --url http://127.0.0.1:<web_port>`) would have put the
    // door on the internet behind the password alone. What came through one says so.
    if ["cf-connecting-ip", "cf-ray", "x-forwarded-for", "forwarded", "x-real-ip"].iter().any(|h| header(head, h).is_some()) {
        return ("403 Forbidden", "only from this machine, not through a tunnel or proxy".into());
    }
    if !head.starts_with("POST ") {
        return ("405 Method Not Allowed", "POST admin commands, one a line".into());
    }
    i.local_admin_failures.retain(|t| t.elapsed() < ADMIN_LOCK_WINDOW);
    if i.local_admin_failures.len() >= ADMIN_LOCK_AFTER {
        return ("429 Too Many Requests", "too many wrong passwords: try again later".into());
    }
    if !same_secret(header(head, "x-admin-password").unwrap_or(""), &i.local_admin_password) {
        i.local_admin_failures.push(Instant::now());
        return ("401 Unauthorized", "wrong admin password".into());
    }
    let commands: Vec<String> = body.lines().map(str::trim).filter(|l| !l.is_empty()).take(10).map(|l| l.chars().take(200).collect()).collect();
    let n = commands.len();
    i.local_admin_queue.extend(commands);
    ("202 Accepted", format!("{n} command(s) taken"))
}

/// One TCP connection to the gateway: a status request, the icon, or a player's WebSocket.
fn serve(stream: TcpStream, target: SocketAddr, info: &Mutex<ServerInfo>, stop: &AtomicBool, connected: &AtomicUsize) -> Result<(), String> {
    let peer = stream.peer_addr().ok();
    stream.set_nonblocking(false).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(Duration::from_secs(10))).map_err(|e| e.to_string())?;
    let mut head = [0u8; 2048];
    let n = stream.peek(&mut head).map_err(|e| e.to_string())?;
    let req = String::from_utf8_lossy(&head[..n]).to_string();
    let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
    let upgrade = req.to_ascii_lowercase().contains("upgrade: websocket");
    if !upgrade {
        let mut s = stream;
        // (the request is read off the socket before the answer: some proxies wait)
        let got = s.read(&mut head).unwrap_or(0);
        let mut request = head[..got].to_vec();
        let (status, ctype, body): (&str, &str, Vec<u8>) = match path.as_str() {
            "/admin" => {
                read_body(&mut s, &mut request);
                let (status, text) = local_admin(&request, peer, info);
                (status, "text/plain; charset=utf-8", text.into_bytes())
            }
            "/status" | "/status.json" => ("200 OK", "application/json", info.lock().unwrap_or_else(|e| e.into_inner()).to_json().into_bytes()),
            "/players" | "/players.json" => {
                let i = info.lock().unwrap_or_else(|e| e.into_inner());
                if i.players_public {
                    ("200 OK", "application/json", players_json(&i.player_list).into_bytes())
                } else {
                    ("404 Not Found", "text/plain", b"this server does not share its players' positions".to_vec())
                }
            }
            "/icon.png" => {
                let icon = info.lock().unwrap_or_else(|e| e.into_inner()).icon.clone();
                if icon.is_empty() {
                    ("404 Not Found", "text/plain", b"no icon".to_vec())
                } else {
                    ("200 OK", "image/png", icon)
                }
            }
            _ => {
                let i = info.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let page = format!("<!doctype html><meta charset=utf-8><title>{0}</title><body style=\"font-family:sans-serif;background:#16181c;color:#eee;padding:40px\"><h1>{0}</h1><p>{1}</p><p>Map: {2} &middot; {3}/{4} players</p><p>Add this address in openOMSI &rarr; Multiplayer &rarr; Servers.</p>", html(&i.name), html(&i.motd), html(&i.map), i.players, i.max_players);
                ("200 OK", "text/html; charset=utf-8", page.into_bytes())
            }
        };
        let hdr = format!("HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n", body.len());
        s.write_all(hdr.as_bytes()).map_err(|e| e.to_string())?;
        s.write_all(&body).map_err(|e| e.to_string())?;
        return Ok(());
    }
    if path.starts_with("/tcp") {
        // a byte stream to the session's TCP port (the host's mods): the way the files go
        // where only HTTP gets through
        let mut ws = tungstenite::accept(stream).map_err(|e| e.to_string())?;
        // (the 10 s of the request's read held every chunk that long)
        ws.get_mut().set_read_timeout(Some(Duration::from_millis(5))).map_err(|e| e.to_string())?;
        let tcp = TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], target.port()))).map_err(|e| e.to_string())?;
        return pump_tcp(&mut ws, tcp, stop);
    }
    let mut ws = tungstenite::accept(stream).map_err(|e| e.to_string())?;
    let udp = UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    udp.set_nonblocking(true).map_err(|e| e.to_string())?;
    // the session sees this player at the socket's 127.0.0.1 address ("joined from
    // 127.0.0.1:<port>"): say who that is, for a server's operator
    if let Ok(local) = udp.local_addr() {
        log::info!("gateway: player WebSocket from {} on {local}", client_addr(&req, peer));
    }
    ws.get_mut().set_read_timeout(Some(Duration::from_millis(5))).map_err(|e| e.to_string())?;
    connected.fetch_add(1, Ordering::Relaxed);
    let r = pump(&mut ws, &udp, |u, d| u.send_to(d, target).map(|_| ()), stop);
    connected.fetch_sub(1, Ordering::Relaxed);
    r
}

fn html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Carry datagrams both ways between a WebSocket and a UDP socket until either side goes.
/// `send` hands a message from the WebSocket to the UDP side.
fn pump<S: Read + Write>(ws: &mut WebSocket<S>, udp: &UdpSocket, mut send: impl FnMut(&UdpSocket, &[u8]) -> std::io::Result<()>, stop: &AtomicBool) -> Result<(), String> {
    let mut buf = vec![0u8; 2048];
    let mut last_in = Instant::now();
    let mut last_ping = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let mut idle = true;
        // WebSocket → UDP
        loop {
            match ws.read() {
                Ok(Message::Binary(d)) => {
                    last_in = Instant::now();
                    idle = false;
                    let _ = send(udp, &d);
                }
                Ok(Message::Close(_)) => return Ok(()),
                Ok(_) => last_in = Instant::now(),
                Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => break,
                Err(tungstenite::Error::ConnectionClosed) | Err(tungstenite::Error::AlreadyClosed) => return Ok(()),
                Err(e) => return Err(e.to_string()),
            }
        }
        // UDP → WebSocket
        loop {
            match udp.recv_from(&mut buf) {
                Ok((n, _)) => {
                    idle = false;
                    if let Err(e) = ws.send(Message::Binary(buf[..n].to_vec().into())) {
                        if !matches!(&e, tungstenite::Error::Io(io) if io.kind() == ErrorKind::WouldBlock) {
                            return Err(e.to_string());
                        }
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::ConnectionReset => break,
                Err(e) => return Err(e.to_string()),
            }
        }
        let _ = ws.flush();
        // (proxies close a WebSocket that stays quiet for a minute or two)
        if last_ping.elapsed() > Duration::from_secs(20) {
            last_ping = Instant::now();
            let _ = ws.send(Message::Ping(Vec::new().into()));
        }
        if last_in.elapsed() > Duration::from_secs(90) {
            return Err("nothing heard for 90 s".into());
        }
        if idle {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    let _ = ws.close(None);
    Ok(())
}

/// Carry a TCP stream both ways over a WebSocket until either side closes.
fn pump_tcp<S: Read + Write>(ws: &mut WebSocket<S>, mut tcp: TcpStream, stop: &AtomicBool) -> Result<(), String> {
    tcp.set_nonblocking(true).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut last = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let mut idle = true;
        loop {
            match ws.read() {
                Ok(Message::Binary(d)) => {
                    idle = false;
                    last = Instant::now();
                    tcp.set_nonblocking(false).ok();
                    let r = tcp.write_all(&d);
                    tcp.set_nonblocking(true).ok();
                    r.map_err(|e| e.to_string())?;
                }
                Ok(Message::Close(_)) => return Ok(()),
                Ok(_) => {}
                Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => break,
                Err(tungstenite::Error::ConnectionClosed) | Err(tungstenite::Error::AlreadyClosed) => return Ok(()),
                Err(e) => return Err(e.to_string()),
            }
        }
        // (up to 2 MB of what the TCP side has before the WebSocket is looked at again: one
        // chunk per turn held a download to a few hundred kB/s)
        for _ in 0..32 {
            match tcp.read(&mut buf) {
                Ok(0) => {
                    let _ = ws.close(None);
                    let _ = ws.flush();
                    return Ok(());
                }
                Ok(n) => {
                    idle = false;
                    last = Instant::now();
                    ws.write(Message::Binary(buf[..n].to_vec().into())).map_err(|e| e.to_string())?;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.to_string()),
            }
        }
        let _ = ws.flush();
        if last.elapsed() > Duration::from_secs(120) {
            return Err("stream idle for 2 min".into());
        }
        if idle {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    Ok(())
}

/// A local TCP port whose connections go over WebSockets to `url` (`wss://…/tcp`): the
/// host's mods fetched through its tunnel.
pub fn tcp_forward(url: &str) -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let url = url.to_string();
    std::thread::Builder::new().name("ws tcp forward".into()).spawn(move || {
        for conn in listener.incoming().flatten() {
            let url = url.clone();
            std::thread::spawn(move || {
                let r = (|| -> Result<(), String> {
                    let (mut ws, _) = tungstenite::connect(&url).map_err(|e| format!("{url}: {e}"))?;
                    match ws.get_mut() {
                        tungstenite::stream::MaybeTlsStream::Plain(s) => s.set_read_timeout(Some(Duration::from_millis(5))),
                        tungstenite::stream::MaybeTlsStream::Rustls(s) => s.get_mut().set_read_timeout(Some(Duration::from_millis(5))),
                        _ => Ok(()),
                    }
                    .map_err(|e| e.to_string())?;
                    pump_tcp(&mut ws, conn, &AtomicBool::new(false))
                })();
                if let Err(e) = r {
                    log::warn!("ws tcp forward: {e}");
                }
            });
        }
    })?;
    Ok(addr)
}

/// A joining game's side (see the module).
pub struct WsClient {
    /// Where the game sends its datagrams (the session "host" as the game sees it).
    pub local: SocketAddr,
    stop: Arc<AtomicBool>,
    pub alive: Arc<AtomicBool>,
    /// How often the bridge opened a new WebSocket after a break (the game says hello again
    /// at once instead of waiting for the host to be missed).
    reconnects: Arc<AtomicUsize>,
}

impl Drop for WsClient {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl WsClient {
    /// One WebSocket to `url`, with the short read timeout the bridge loop needs (a dead
    /// tunnel must not hold the game for ever).
    fn open(url: &str) -> Result<WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let u = url.to_string();
        std::thread::spawn(move || {
            let _ = tx.send(tungstenite::connect(&u).map(|x| x.0).map_err(|e| format!("{u}: {e}")));
        });
        let mut ws = rx.recv_timeout(Duration::from_secs(12)).map_err(|_| format!("{url}: no answer within 12 s"))??;
        match ws.get_mut() {
            tungstenite::stream::MaybeTlsStream::Plain(s) => s.set_read_timeout(Some(Duration::from_millis(5))),
            tungstenite::stream::MaybeTlsStream::Rustls(s) => s.get_mut().set_read_timeout(Some(Duration::from_millis(5))),
            _ => Ok(()),
        }
        .map_err(|e| e.to_string())?;
        Ok(ws)
    }

    /// Connect to `url` (`wss://…/ws`) and give the local address to join. When the
    /// connection breaks later, the bridge opens a new one by itself and keeps the local
    /// address: the game's session finds its way back without a restart (the server knows
    /// the player again by its nonce).
    pub fn connect(url: &str) -> Result<WsClient, String> {
        let mut ws = Self::open(url)?;
        let udp = UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        udp.set_nonblocking(true).map_err(|e| e.to_string())?;
        let local = udp.local_addr().map_err(|e| e.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let reconnects = Arc::new(AtomicUsize::new(0));
        let (st, al, rc) = (stop.clone(), alive.clone(), reconnects.clone());
        let url = url.to_string();
        std::thread::Builder::new()
            .name("ws client".into())
            .spawn(move || {
                // the game's own socket: replies go to wherever it sent from
                let game: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));
                let udp2 = udp.try_clone().expect("udp clone");
                let g2 = game.clone();
                // (the game's datagrams are read in `pump`'s UDP half; the address they came
                // from is where the WebSocket's answers go)
                let mut buf = vec![0u8; 2048];
                let mut last_in = Instant::now();
                let mut last_ping = Instant::now();
                while !st.load(Ordering::Relaxed) {
                    let r: Result<(), String> = (|| {
                        while !st.load(Ordering::Relaxed) {
                            let mut idle = true;
                            loop {
                                match ws.read() {
                                    Ok(Message::Binary(d)) => {
                                        last_in = Instant::now();
                                        idle = false;
                                        if let Some(to) = *g2.lock().unwrap_or_else(|e| e.into_inner()) {
                                            let _ = udp2.send_to(&d, to);
                                        }
                                    }
                                    Ok(Message::Close(_)) => return Err("the server closed the connection".into()),
                                    Ok(_) => last_in = Instant::now(),
                                    Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => break,
                                    Err(tungstenite::Error::ConnectionClosed) | Err(tungstenite::Error::AlreadyClosed) => return Err("the connection was closed".into()),
                                    Err(e) => return Err(e.to_string()),
                                }
                            }
                            loop {
                                match udp.recv_from(&mut buf) {
                                    Ok((n, from)) => {
                                        idle = false;
                                        *game.lock().unwrap_or_else(|e| e.into_inner()) = Some(from);
                                        if let Err(e) = ws.send(Message::Binary(buf[..n].to_vec().into())) {
                                            if !matches!(&e, tungstenite::Error::Io(io) if io.kind() == ErrorKind::WouldBlock) {
                                                return Err(e.to_string());
                                            }
                                        }
                                    }
                                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                                    Err(e) if e.kind() == ErrorKind::ConnectionReset => break,
                                    Err(e) => return Err(e.to_string()),
                                }
                            }
                            let _ = ws.flush();
                            if last_ping.elapsed() > Duration::from_secs(20) {
                                last_ping = Instant::now();
                                let _ = ws.send(Message::Ping(Vec::new().into()));
                            }
                            if last_in.elapsed() > Duration::from_secs(90) {
                                return Err("the server has been silent for 90 s".into());
                            }
                            if idle {
                                std::thread::sleep(Duration::from_millis(2));
                            }
                        }
                        let _ = ws.close(None);
                        Ok(())
                    })();
                    if st.load(Ordering::Relaxed) {
                        break;
                    }
                    if let Err(e) = r {
                        log::warn!("ws client {url}: {e}; connecting again");
                    }
                    // a new connection, with a pause that grows to a few seconds
                    let mut wait = 500u64;
                    loop {
                        if st.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(wait));
                        match Self::open(&url) {
                            Ok(w) => {
                                ws = w;
                                // what the game said while the way was down is stale
                                while udp.recv_from(&mut buf).is_ok() {}
                                last_in = Instant::now();
                                last_ping = Instant::now();
                                log::info!("ws client {url}: connected again");
                                rc.fetch_add(1, Ordering::Relaxed);
                                break;
                            }
                            Err(e) => {
                                log::info!("ws client: still no connection ({e})");
                                wait = (wait * 2).min(5000);
                            }
                        }
                    }
                }
                al.store(false, Ordering::Relaxed);
            })
            .map_err(|e| e.to_string())?;
        log::info!("ws client: the session is reached through {local}");
        Ok(WsClient { local, stop, alive, reconnects })
    }

    /// How many times the connection was made again after a break.
    pub fn reconnects(&self) -> usize {
        self.reconnects.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(ws_url("https://abc.trycloudflare.com").as_deref(), Some("wss://abc.trycloudflare.com/ws"));
        assert_eq!(ws_url("abc.trycloudflare.com").as_deref(), Some("wss://abc.trycloudflare.com/ws"));
        assert_eq!(ws_url("http://10.0.0.2:27025/").as_deref(), Some("ws://10.0.0.2:27025/ws"));
        assert_eq!(ws_url("192.168.1.4:27015"), None);
        assert_eq!(http_base("https://abc.trycloudflare.com").as_deref(), Some("https://abc.trycloudflare.com"));
        assert_eq!(web_bases("192.168.1.4:27015"), ["http://192.168.1.4:27015", "http://192.168.1.4:27025"]);
        assert_eq!(web_bases("play.example.org"), ["http://play.example.org:27025", "https://play.example.org", "http://play.example.org"]);
        assert_eq!(web_bases("::1")[0], "http://[::1]:27025");
        assert_eq!(web_bases("[::1]:27025"), ["http://[::1]:27025", "http://[::1]:27035"]);
    }

    #[test]
    fn status_round_trip() {
        let i = ServerInfo { name: "Spandau \"1\"".into(), motd: "hi".into(), map: "maps/Berlin-Spandau/global.cfg".into(), players: 2, max_players: 16, version: "0.1".into(), icon: vec![1, 2], time: "08:00".into(), weather: "Sommerlich".into(), password: false, vehicles: vec!["Vehicles/MAN_SD202/SD202.bus".into()], ..Default::default() };
        let j = i.to_json();
        let b = ServerInfo::from_json(&j).unwrap();
        assert_eq!(b.name, i.name);
        assert_eq!(b.vehicles, i.vehicles);
        assert_eq!(b.players, 2);
        assert_eq!(b.max_players, 16);
        assert!(!b.icon.is_empty());
    }

    #[test]
    fn a_websocket_behind_a_proxy_is_told_by_the_forwarded_address() {
        let local: SocketAddr = "127.0.0.1:50000".parse().unwrap();
        let far: SocketAddr = "203.0.113.7:50000".parse().unwrap();
        let req = |h: &str| format!("GET / HTTP/1.1\r\nHost: x\r\n{h}Upgrade: websocket\r\n\r\n");
        assert_eq!(client_addr(&req("X-Forwarded-For: 198.51.100.4, 10.0.0.1\r\n"), Some(local)), "198.51.100.4");
        assert_eq!(client_addr(&req("X-Real-IP: 2001:db8::1\r\n"), Some(local)), "2001:db8::1");
        assert_eq!(client_addr(&req("CF-Connecting-IP: 192.0.2.9\r\nX-Forwarded-For: 198.51.100.4\r\n"), Some(local)), "192.0.2.9");
        // no proxy: the peer; a header from the internet is not believed
        assert_eq!(client_addr(&req(""), Some(local)), "127.0.0.1");
        assert_eq!(client_addr(&req("X-Forwarded-For: 198.51.100.4\r\n"), Some(far)), "203.0.113.7");
        assert_eq!(client_addr(&req("X-Forwarded-For: not-an-address\r\n"), Some(local)), "127.0.0.1");
        assert_eq!(client_addr(&req(""), None), "?");
    }

    #[test]
    fn status_counts_a_servers_world() {
        let mut i = ServerInfo { name: "NEROSY".into(), players: 2, vehicles: vec!["Vehicles/A/a.bus".into()], ..Default::default() };
        assert!(i.to_json().ends_with(",\"world\":null}"));
        i.world = Some(WorldCounts { cars: 41, buses: 7, dormant: 12, parked: 230, walking: 55, waiting: 18, aboard: 9, traffic: 30 });
        let j = i.to_json();
        assert!(j.ends_with(",\"world\":{\"cars\":41,\"buses\":7,\"dormant\":12,\"parked\":230,\"walking\":55,\"waiting\":18,\"aboard\":9,\"traffic\":30}}"), "{j}");
        // the launcher still reads the rest (the world's keys are none of the server's own)
        let back = ServerInfo::from_json(&j).unwrap();
        assert_eq!((back.name.as_str(), back.players, back.vehicles.len()), ("NEROSY", 2, 1));
    }

    #[test]
    fn local_admin_door_is_shut_to_tunnels() {
        let info = Mutex::new(ServerInfo { local_admin_password: "s3cret".into(), ..Default::default() });
        let req = b"POST /admin HTTP/1.1\r\nX-Admin-Password: s3cret\r\nCf-Connecting-Ip: 203.0.113.9\r\nContent-Length: 6\r\n\r\nsay hi";
        assert_eq!(local_admin(req, Some(SocketAddr::from(([127, 0, 0, 1], 5000))), &info).0, "403 Forbidden");
        assert!(info.lock().unwrap().local_admin_queue.is_empty());
    }

    #[test]
    fn local_admin_door() {
        let info = Mutex::new(ServerInfo::default());
        let here = Some(SocketAddr::from(([127, 0, 0, 1], 5000)));
        let post = |pw: &str, body: &str| format!("POST /admin HTTP/1.1\r\nHost: x\r\nX-Admin-Password: {pw}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).into_bytes();
        // no password: no door
        assert_eq!(local_admin(&post("", "say hi"), here, &info).0, "404 Not Found");
        info.lock().unwrap().local_admin_password = "s3cret".into();
        // not from this machine
        assert_eq!(local_admin(&post("s3cret", "say hi"), Some(SocketAddr::from(([10, 0, 0, 2], 5000))), &info).0, "403 Forbidden");
        assert_eq!(local_admin(b"GET /admin HTTP/1.1\r\nX-Admin-Password: s3cret\r\n\r\n", here, &info).0, "405 Method Not Allowed");
        assert_eq!(local_admin(&post("s3cre", "say hi"), here, &info).0, "401 Unauthorized");
        assert!(info.lock().unwrap().local_admin_queue.is_empty());
        let (st, _) = local_admin(&post("s3cret", "clock 30600\r\n\r\nweather set Weather/#CAVOK.owt\n"), here, &info);
        assert_eq!(st, "202 Accepted");
        assert_eq!(info.lock().unwrap().local_admin_queue, ["clock 30600", "weather set Weather/#CAVOK.owt"]);
        // five wrong passwords close the door, the right one included
        for _ in 0..4 {
            local_admin(&post("nope", "say x"), here, &info);
        }
        assert_eq!(local_admin(&post("s3cret", "say x"), here, &info).0, "429 Too Many Requests");
        assert!(same_secret("abc", "abc") && !same_secret("abc", "abd") && !same_secret("abc", "abcd") && !same_secret("", "a"));
    }

    #[test]
    fn players_list() {
        let p = PlayerInfo { id: 3, name: "Anna \"A\"".into(), bus: "Vehicles/MAN_SD200/MAN_SD77.bus".into(), line: "37".into(), x: 894179.74, y: 4196165.3, heading: 200.0, speed_kmh: 31.25, lat_lon: Some((52.535412, 13.199642)), ..Default::default() };
        let j = players_json(&[p.clone(), PlayerInfo { id: 4, x: f64::NAN, ..Default::default() }]);
        assert!(j.starts_with("[{\"id\":3,\"name\":\"Anna \\\"A\\\"\""), "{j}");
        assert!(j.contains("\"line\":\"37\""), "{j}");
        assert!(j.contains("\"x\":894179.7,"), "{j}");
        assert!(j.contains("\"on_foot\":false,\"aboard\":null,\"lat\":52.535412,\"lon\":13.199642}"), "{j}");
        assert!(j.contains("\"id\":4,") && j.contains("\"x\":null") && j.ends_with("\"lat\":null,\"lon\":null}]"), "{j}");
        assert_eq!(players_json(&[]), "[]");
    }

    #[test]
    fn the_client_connects_again_when_the_way_breaks() {
        // a stand-in server: the first connection echoes one datagram and drops, the second
        // is a good one
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for round in 0..2 {
                let Ok((s, _)) = l.accept() else { return };
                let Ok(mut ws) = tungstenite::accept(s) else { return };
                loop {
                    match ws.read() {
                        Ok(Message::Binary(d)) => {
                            let _ = ws.send(Message::Binary(d));
                            if round == 0 {
                                break;
                            }
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
            }
        });
        let client = WsClient::connect(&format!("ws://{addr}/ws")).unwrap();
        let game = UdpSocket::bind("127.0.0.1:0").unwrap();
        game.set_read_timeout(Some(Duration::from_millis(250))).unwrap();
        let mut got = [0u8; 64];
        let mut first = false;
        for _ in 0..20 {
            game.send_to(b"ONE", client.local).unwrap();
            if let Ok((n, _)) = game.recv_from(&mut got) {
                first = &got[..n] == b"ONE";
                break;
            }
        }
        assert!(first, "the first connection works");
        let mut again = false;
        for _ in 0..40 {
            game.send_to(b"TWO", client.local).unwrap();
            if let Ok((n, _)) = game.recv_from(&mut got) {
                if &got[..n] == b"TWO" {
                    again = true;
                    break;
                }
            }
        }
        assert!(again, "the datagram came back through the second connection");
        assert!(client.alive.load(Ordering::Relaxed));
        assert_eq!(client.reconnects(), 1);
    }

    #[test]
    fn datagrams_go_both_ways() {
        // a stand-in session: echoes every datagram
        let echo = UdpSocket::bind("127.0.0.1:0").unwrap();
        let target = echo.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut b = [0u8; 2048];
            loop {
                if let Ok((n, from)) = echo.recv_from(&mut b) {
                    let _ = echo.send_to(&b[..n], from);
                }
            }
        });
        let gw = WsGateway::start("127.0.0.1:0".parse().unwrap(), target, ServerInfo { name: "t".into(), ..Default::default() }).unwrap();
        let client = WsClient::connect(&format!("ws://{}/ws", gw.addr)).unwrap();
        let game = UdpSocket::bind("127.0.0.1:0").unwrap();
        game.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut got = [0u8; 64];
        let mut ok = false;
        for _ in 0..30 {
            game.send_to(b"HELLO", client.local).unwrap();
            if let Ok((n, _)) = game.recv_from(&mut got) {
                ok = &got[..n] == b"HELLO";
                break;
            }
        }
        assert!(ok, "the datagram came back through the WebSocket");
        let st = query(&format!("http://{}", gw.addr), false).unwrap();
        assert_eq!(st.name, "t");
        // the admin door over a real connection: a body sent after the head is read too
        gw.info.lock().unwrap().local_admin_password = "pw".into();
        let mut s = TcpStream::connect(gw.addr).unwrap();
        s.write_all(b"POST /admin HTTP/1.1\r\nHost: x\r\nX-Admin-Password: pw\r\nContent-Length: 11\r\n\r\n").unwrap();
        std::thread::sleep(Duration::from_millis(50));
        s.write_all(b"say hello\r\n").unwrap();
        let mut r = String::new();
        let _ = s.read_to_string(&mut r);
        assert!(r.starts_with("HTTP/1.1 202"), "{r}");
        assert_eq!(gw.info.lock().unwrap().local_admin_queue, ["say hello"]);
        // the players' positions only when the server shares them
        let get = |path: &str| {
            let mut s = TcpStream::connect(gw.addr).unwrap();
            s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).unwrap();
            let mut r = String::new();
            let _ = s.read_to_string(&mut r);
            r
        };
        assert!(get("/players").starts_with("HTTP/1.1 404"));
        gw.info.lock().unwrap().players_public = true;
        gw.info.lock().unwrap().player_list = vec![PlayerInfo { id: 1, name: "p".into(), ..Default::default() }];
        let r = get("/players");
        assert!(r.starts_with("HTTP/1.1 200") && r.ends_with("\"lat\":null,\"lon\":null}]"), "{r}");
    }
}
