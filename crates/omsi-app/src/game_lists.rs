//! The game menu's own windows besides the administration (see `admin`), as OMSI has them
//! in its menus: the options that can change while driving, the line and tour to drive,
//! the driver whose personnel file the run goes into, and the bus's fleet number. Each is a
//! list of (label, action) lines in the menu's chooser; choosing a line does it and shows
//! the list again (or the next one: a line's tours).

use crate::App;

/// Which list the chooser shows.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ListKind {
    Admin,
    /// The options of the game, on this page of the settings window.
    Options(usize),
    /// What can be done with the vehicle, on this page.
    Vehicle(usize),
    /// The clock, the weather and the traffic, on this page.
    World(usize),
    Lines,
    /// A line's tours; the stop chosen in the timetable beside them to start from: (the
    /// tour's number, the stop as `Schedule::tour_stops` lists them), none: the default.
    Tours(String, Option<(String, usize, usize)>),
    Drivers,
    Numbers,
    /// The termini of the bus's depot file, for its destination display.
    Destinations,
    /// The route numbers (lines) for the destination display: the depot file's and the
    /// map timetable's.
    RouteNumbers,
    /// The depot files (.hof) of the bus driven.
    Hofs,
    /// The map's entry points (the launcher's "Start at"), to put the bus at.
    Spots,
    /// Placing a vehicle: its manufacturer, then its type (the manufacturer's key), its
    /// livery, then its depot file (bus file; bus file and livery).
    PlaceMaker,
    PlaceType(String),
    PlaceLivery(String),
    PlaceHof(String, String),
}

/// A vehicle file of the menu's list (`Vehicles/...`) as its definition.
fn bus_def(app: &App, bus: &str) -> Option<omsi_vehicle::Vehicle> {
    let path = crate::spawn::player_bus_path(&app.args.root, bus).ok()?;
    omsi_vehicle::Vehicle::load(&path).ok()
}

/// The route numbers to choose from: the lines of the depot file's routes (their own
/// line, or the route code without its last two digits) and of the map's timetable.
fn route_numbers(app: &App) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(hof) = app.player.as_ref().and_then(|p| p.vehicle.host.hof.clone()) {
        for t in &hof.info_trips {
            let code = t.code.trim();
            let l = if !t.line.trim().is_empty() { t.line.trim().to_string() } else if code.len() > 2 && code.chars().all(|c| c.is_ascii_digit()) { code[..code.len() - 2].trim_start_matches('0').to_string() } else { String::new() };
            // A route number is display text, not necessarily an alphanumeric IBIS code:
            // Brazilian matrices use values such as "-10". Keep the HOF spelling intact.
            let l = l.trim().to_string();
            if !l.is_empty() && !out.contains(&l) {
                out.push(l);
            }
        }
    }
    if let Some(sch) = app.schedule.as_ref() {
        for l in &sch.data.lines {
            let n = l.name.trim().to_string();
            if !n.is_empty() && !out.contains(&n) {
                out.push(n);
            }
        }
    }
    out.sort_by(|a, b| natural(a, b));
    out
}

/// Whether a line can be represented by the numeric IBIS line/suffix variables openOMSI
/// already knows how to encode. Everything else must stay as display text: turning "-10"
/// or "EXP" into a number loses information.
fn numeric_ibis_line(line: &str) -> bool {
    let line = line.trim();
    if line.is_empty() {
        return false;
    }
    if line.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    let mut chars = line.chars();
    if let Some(first) = chars.next() {
        let rest: String = chars.collect();
        if matches!(first.to_ascii_uppercase(), 'E' | 'S' | 'A' | 'D' | 'C' | 'B' | 'U' | 'M' | 'N' | 'X')
            && !rest.is_empty()
            && rest.chars().all(|c| c.is_ascii_digit())
        {
            return true;
        }
    }
    let digits: String = line.chars().take_while(|c| c.is_ascii_digit()).collect();
    let suffix = &line[digits.len()..];
    !digits.is_empty()
        && suffix.chars().count() == 1
        && matches!(suffix.chars().next().unwrap().to_ascii_uppercase(), 'E' | 'U' | 'N' | 'S' | 'M')
}

/// The route number on the bus's IBIS and display, as picked or typed in the destination
/// list (the destination stays: the one on the display now, else the first).
pub(crate) fn set_route_by_hand(app: &mut App, line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    if let Some(p) = app.player.as_mut() {
        if numeric_ibis_line(line) {
            let hof = p.vehicle.host.hof.clone();
            let code = p.vehicle.var("IBIS_TerminusCode").unwrap_or(-1.0) as i32;
            let named = |t: &&omsi_vehicle::hof::Terminus| t.strings.first().is_some_and(|s| !s.trim().is_empty());
            let term = hof.as_ref().and_then(|h| h.termini.iter().filter(named).find(|t| t.code == code).or_else(|| h.termini.iter().find(named)));
            let name = term.and_then(|t| t.strings.first().cloned()).unwrap_or_default();
            crate::schedule::set_player_destination_directly(&mut p.vehicle, hof.as_deref(), line, &name, &[]);
        } else {
            // OMSI's route-number field is also used as arbitrary display text. Do not
            // force symbols/unknown letters through IBIS_LinieKurs: that would turn "-10"
            // into line 0 (and matrix scripts would then blank or rewrite the rear sign).
            for name in ["SetLineTo", "Matrix_Nr", "Linie"] {
                if let Some(i) = p.vehicle.ty.program.str_var(name) {
                    p.vehicle.state.str_vars[i as usize] = line.to_string();
                }
            }
        }
        log::info!(
            "route number set by hand: {line} (IBIS_LinieKurs {:?}, Matrix_Nr {:?})",
            p.vehicle.var("IBIS_LinieKurs"),
            p.vehicle.str_var("Matrix_Nr")
        );
        app.service_msg = Some((format!("Route {line}"), 3.0));
    }
}

/// Names in older packs often use underscores as spaces.
fn bus_label(name: &str) -> String {
    name.replace('_', " ").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Numbers inside names sort as numbers (DL9 before DL10), case does not matter.
fn bus_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (a, b) = (a.to_lowercase(), b.to_lowercase());
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let x: String = std::iter::from_fn(|| a.next_if(|c| c.is_ascii_digit())).collect();
                let y: String = std::iter::from_fn(|| b.next_if(|c| c.is_ascii_digit())).collect();
                let (x, y) = (x.trim_start_matches('0'), y.trim_start_matches('0'));
                let order = x.len().cmp(&y.len()).then_with(|| x.cmp(y));
                if order != Ordering::Equal {
                    return order;
                }
            }
            (Some(x), Some(y)) => {
                let order = x.cmp(&y);
                if order != Ordering::Equal {
                    return order;
                }
                a.next();
                b.next();
            }
        }
    }
}

/// The vehicles of the place list as (manufacturer's key, manufacturer, type, path).
fn place_vehicles(app: &App, unknown: &str) -> Vec<(String, String, String, String)> {
    app.vehicle_list
        .iter()
        .map(|(name, path)| {
            let (maker, ty) = app.vehicle_meta.get(path).cloned().unwrap_or_default();
            let maker = bus_label(&maker);
            let ty = if ty.trim().is_empty() { bus_label(name) } else { bus_label(&ty) };
            let shown = if maker.is_empty() { unknown.to_string() } else { maker.clone() };
            (maker.to_lowercase(), shown, ty, path.clone())
        })
        .collect()
}

/// The paint schemes of a vehicle file, by name (without loading its meshes).
fn liveries(def: &omsi_vehicle::Vehicle) -> Vec<String> {
    let Some(m) = def.model.as_ref() else { return Vec::new() };
    let mp = omsi_cfg::resolve_path(def.dir(), m);
    let Ok(model) = omsi_model::Model::load(&mp) else { return Vec::new() };
    let mut names: Vec<String> = model.ctc.iter().flat_map(|c| omsi_sim::vehicle::load_paint_schemes(&omsi_cfg::resolve_path(def.dir(), &c.path))).map(|s| s.name).collect();
    names.dedup();
    names
}

/// A depot file's name for the lists: its `[name]`, and the file.
fn hof_label(p: &std::path::Path) -> String {
    let file = p.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    match omsi_vehicle::Hof::load(p).ok().map(|h| h.name).filter(|n| !n.trim().is_empty()) {
        Some(n) if !file.to_ascii_lowercase().starts_with(&n.trim().to_ascii_lowercase()) => format!("{}  ({file})", n.trim()),
        _ => file,
    }
}

/// The time speeds, traffic amounts and passenger shares the options step through.
const SPEEDS: [f64; 5] = [1.0, 2.0, 4.0, 8.0, 15.0];
pub(crate) const TRAFFIC: [usize; 7] = [0, 10, 20, 30, 50, 80, 120];
const PAX: [f32; 6] = [0.25, 0.5, 0.75, 1.0, 1.5, 2.0];
const VOLUME: [f32; 6] = [0.0, 0.2, 0.4, 0.6, 0.8, 1.0];
/// The pedal strengths the options step through (see `settings::pedal_curve`).
const PEDAL: [f32; 7] = [0.5, 0.7, 0.85, 1.0, 1.25, 1.5, 2.0];

/// The next of `steps` after `now` (round to the first).
pub(crate) fn next_step<T: PartialOrd + Copy>(steps: &[T], now: T) -> T {
    steps.iter().copied().find(|s| *s > now).unwrap_or(steps[0])
}

/// A list line that heads the lines under it: not chosen, not run.
pub(crate) const HEADING: &str = "#";
/// The end of the action of a line whose value Left and Right step down and up (and the
/// arrows drawn round its value): the action is run with `-` or `+` in its place, and with
/// it as it is on Enter (see `App::chooser_adjust`).
pub(crate) const ADJUST: &str = " ±";

pub(crate) fn items(app: &App, kind: &ListKind) -> Vec<(String, String)> {
    let tr = |t: &str| omsi_ui::tr(t).into_owned();
    let mut out: Vec<(String, String)> = Vec::new();
    match kind {
        ListKind::Admin => return crate::admin::items(app),
        ListKind::Options(_) | ListKind::Vehicle(_) | ListKind::World(_) => {
            let Some((mut pages, tab)) = pages_of(app, kind) else { return out };
            if pages.is_empty() {
                return vec![(row("Nothing to set here", 'i', "", "", None), "noop".to_string())];
            }
            return pages.swap_remove(tab).1;
        }
        ListKind::Lines => {
            if let Some(sch) = app.schedule.as_ref() {
                let mut lines: Vec<&omsi_timetable::Line> = sch.data.lines.iter().filter(|l| l.user_allowed && l.tours.iter().any(|t| tour_listed(sch, &l.name, t, app.clock.time))).collect();
                lines.sort_by(|a, b| natural(&a.name, &b.name));
                for l in lines {
                    out.push((format!("{} {}  ({} {})", tr("Line"), l.name, l.tours.iter().filter(|t| tour_listed(sch, &l.name, t, app.clock.time)).count(), tr("tours")), format!("line {}", l.name)));
                }
            }
            if out.is_empty() {
                out.push((tr("No timetable on this map"), "back".into()));
            }
        }
        ListKind::Tours(line, _) => {
            if let Some(l) = app.schedule.as_ref().and_then(|s| s.data.lines.iter().find(|l| l.name == *line)) {
                for t in sorted_tours(l).into_iter().filter(|t| app.schedule.as_ref().is_some_and(|s| tour_listed(s, line, t, app.clock.time))) {
                    // (the tours in order of the time they start)
                    out.push((format!("{} {}", tr("Tour"), t.number.trim()), format!("tour {}\u{1}{}", line, t.number)));
                }
            }
        }
        ListKind::Drivers => {
            for name in driver_names(app) {
                let mark = if app.career.path.as_ref().and_then(|p| p.file_stem()).is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case(&name)) { format!("  {}", tr("(now)")) } else { String::new() };
                out.push((format!("{name}{mark}"), format!("driver {name}")));
            }
        }
        ListKind::Destinations => {
            if let Some(p) = app.player.as_ref().filter(|p| p.vehicle.host.hof.is_some()) {
                let shown = p.vehicle.str_var("Matrix_Nr").trim().to_string();
                let set = p.vehicle.str_var("SetLineTo").trim().to_string();
                let now = if !shown.is_empty() {
                    shown
                } else if !set.is_empty() {
                    set
                } else {
                    p.vehicle.var("IBIS_LinieKurs").filter(|l| *l > 0.0).map(|l| format!("{}", l as i64)).unwrap_or_else(|| "-".into())
                };
                out.push((format!("{}: {now}...", tr("Route number")), "routes".into()));
            }
            if let Some(hof) = app.player.as_ref().and_then(|p| p.vehicle.host.hof.clone()) {
                let mut termini: Vec<(String, String)> = hof
                    .termini
                    .iter()
                    .map(|t| (t.strings.iter().find(|s| !s.trim().is_empty()).cloned().unwrap_or_else(|| t.code.to_string()), t.code.to_string()))
                    .collect();
                // (alphabetical, by name)
                termini.sort_by_key(|(name, _)| name.trim().to_lowercase());
                for (name, code) in termini {
                    out.push((format!("{:>3}  {}", code, name.trim()), format!("dest {code}")));
                }
            }
            if out.is_empty() {
                out.push((tr("This bus has no depot file (.hof) with destinations"), "back".into()));
            }
        }
        ListKind::RouteNumbers => {
            // any route number, typed as on OMSI's own field (#836): the scripts that read
            // it (a bus that switches its functions by route number) take what is typed
            match app.menu_edit.as_ref() {
                Some(t) => out.push((format!("{}: {t}_  ({})", tr("Route number"), tr("Enter sets it, Esc cancels")), "route_type".into())),
                None => out.push((format!("{}...", tr("Type a route number")), "route_type".into())),
            }
            for l in route_numbers(app) {
                out.push((format!("{} {l}", tr("Route")), format!("route {l}")));
            }
            if out.len() == 1 {
                out.push((tr("No route numbers in the depot file or the timetable"), "back".into()));
            }
        }
        ListKind::Hofs => {
            if let Some(p) = app.player.as_ref() {
                let now = p.vehicle.host.hof.as_ref().map(|h| h.path.clone());
                let mut files: Vec<(String, std::path::PathBuf)> = omsi_vehicle::hof::depot_files(p.vehicle.ty.def.dir()).into_iter().map(|f| (hof_label(&f), f)).collect();
                // (alphabetical)
                files.sort_by_key(|(label, _)| label.to_lowercase());
                for (label, f) in files {
                    let mark = if now.as_ref() == Some(&f) { format!("  {}", tr("(now)")) } else { String::new() };
                    out.push((format!("{label}{mark}"), format!("hof {}", f.to_string_lossy())));
                }
            }
            if out.is_empty() {
                out.push((tr("This bus has no depot files (.hof)"), "back".into()));
            }
        }
        ListKind::Spots => {
            if let Some(w) = app.world.as_ref() {
                // (the map's entry points, as the launcher's "Start at" lists them)
                for (i, e) in w.global.entry_points.iter().enumerate() {
                    let label = if e.name.trim().is_empty() { format!("{} {}", tr("entry"), e.index + 1) } else { e.name.trim().to_string() };
                    out.push((label, format!("spot {i}")));
                }
            }
            if out.is_empty() {
                out.push((tr("This map has no entry points"), "back".into()));
            }
        }
        ListKind::PlaceMaker => {
            let all = place_vehicles(app, &tr("Unknown manufacturer"));
            let mut groups: Vec<(String, String, Vec<&(String, String, String, String)>)> = Vec::new();
            for v in &all {
                match groups.iter_mut().find(|g| g.0 == v.0) {
                    Some(g) => g.2.push(v),
                    None => groups.push((v.0.clone(), v.1.clone(), vec![v])),
                }
            }
            groups.sort_by(|a, b| bus_cmp(&a.1, &b.1).then_with(|| a.0.cmp(&b.0)));
            for (key, name, vs) in groups {
                if vs.len() == 1 {
                    // (a manufacturer with one type: that type at once)
                    out.push((format!("{name}  ·  {}", vs[0].2), format!("bus {}", vs[0].3)));
                } else {
                    out.push((format!("{name}  ({} {})", vs.len(), tr("models")), format!("maker {key}")));
                }
            }
        }
        ListKind::PlaceType(key) => {
            let all = place_vehicles(app, &tr("Unknown manufacturer"));
            let mut types: Vec<(String, String)> = all.iter().filter(|v| v.0 == *key).map(|v| (v.2.clone(), v.3.clone())).collect();
            types.sort_by(|a, b| bus_cmp(&a.0, &b.0).then_with(|| a.1.cmp(&b.1)));
            // (a type name used twice: with its pack's folder, then with its file)
            let same = |t: &[(String, String)], n: &str| t.iter().filter(|x| x.0.to_lowercase() == n.to_lowercase()).count();
            let counts: Vec<usize> = types.iter().map(|t| same(&types, &t.0)).collect();
            for (t, n) in types.iter().zip(counts) {
                let mut label = t.0.clone();
                if n > 1 {
                    let parts: Vec<&str> = t.1.split('/').collect();
                    let folder = parts.get(1).copied().unwrap_or_default();
                    let file = parts.last().copied().unwrap_or_default().rsplit_once('.').map(|x| x.0).unwrap_or_default();
                    label = format!("{label}  ·  {}  ·  {}", bus_label(folder), bus_label(file));
                }
                out.push((label, format!("bus {}", t.1)));
            }
        }
        ListKind::PlaceLivery(bus) => {
            out.push((tr("Random livery"), "livery ".into()));
            let mut names = bus_def(app, bus).map(|d| liveries(&d)).unwrap_or_default();
            // (alphabetical)
            names.sort_by_key(|n| n.to_lowercase());
            names.dedup();
            for n in names {
                out.push((n.clone(), format!("livery {n}")));
            }
        }
        ListKind::PlaceHof(bus, _) => {
            out.push((tr("The map's depot file"), "placehof ".into()));
            if let Some(d) = bus_def(app, bus) {
                let mut files: Vec<(String, String)> = omsi_vehicle::hof::depot_files(d.dir())
                    .into_iter()
                    .map(|f| (hof_label(&f), f.file_name().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default()))
                    .collect();
                // (alphabetical)
                files.sort_by_key(|(label, _)| label.to_lowercase());
                for (label, name) in files {
                    out.push((label, format!("placehof {name}")));
                }
            }
        }
        ListKind::Numbers => {
            if let Some(p) = app.player.as_ref() {
                let mut numbers = fleet_numbers(&p.vehicle);
                // (in order of the numbers)
                numbers.sort_by(|a, b| natural(&a.0, &b.0));
                for (n, reg) in numbers {
                    out.push((if reg.is_empty() { n.clone() } else { format!("{n}  ({reg})") }, format!("number {n}\u{1}{reg}")));
                }
            }
            if out.is_empty() {
                out.push((tr("This bus has no list of fleet numbers"), "back".into()));
            }
        }
    }
    out.push((tr("Back"), "back".into()));
    out
}

/// What the open list tells the interface besides its lines: the layout it wants, its title
/// and the small line above it, and - for lines and tours - the timetable of the chosen one.
pub(crate) fn menu_extras(
    kind: Option<&ListKind>,
    list: Option<&[(String, String)]>,
    sel: Option<usize>,
    schedule: Option<&crate::schedule::Schedule>,
    now: f64,
) -> (crate::ui::MenuKind, Option<(String, String)>, Option<crate::ui::Preview>) {
    use crate::ui::{MenuKind, Preview};
    let Some(sel) = sel else { return (MenuKind::Game, None, None) };
    let tr = |t: &str| omsi_ui::tr(t).into_owned();
    // (the menu's own labels, translated, without their dots)
    let title = |t: &str| tr(t).trim_end_matches("...").trim_end_matches('…').trim_end().to_string();
    let head = |t: &str| Some((title(t), String::new()));
    let hm = |m: f32| format!("{:02}:{:02}", (m / 60.0) as i32 % 24, (m % 60.0) as i32);
    // a trip's line and terminus
    let trip_of = |name: &str| -> (String, String) {
        schedule
            .and_then(|s| s.data.trips.iter().find(|x| x.name.eq_ignore_ascii_case(name)))
            .map(|x| (x.line.trim().to_string(), x.terminus.trim().to_string()))
            .unwrap_or_default()
    };
    let action = list.and_then(|l| l.get(sel)).map(|x| x.1.as_str()).unwrap_or("");
    // (no kind: the vehicle chooser)
    let Some(kind) = kind else { return (MenuKind::List, head("Place a vehicle..."), None) };
    match kind {
        ListKind::Options(_) => (MenuKind::Options, head("Options..."), None),
        ListKind::Vehicle(_) => (MenuKind::Options, head("Vehicle options..."), None),
        ListKind::World(_) => (MenuKind::Options, head("World options..."), None),
        ListKind::Lines => {
            let preview = action.strip_prefix("line ").and_then(|name| {
                let line = schedule?.data.lines.iter().find(|l| l.name == name)?;
                let rows = sorted_tours(line)
                    .into_iter()
                    .filter(|t| schedule.is_some_and(|s| tour_listed(s, &line.name, t, now)))
                    .map(|t| {
                        // (the trip and the time the tour has from the game's time on)
                        let next = schedule.and_then(|s| s.tour_stops_from(&line.name, &t.number, now).first().cloned());
                        let end = match (schedule, next.as_ref()) {
                            (Some(s), Some(n)) => tour_trip_name(s, t, n.0).map(|name| trip_of(&name).1).unwrap_or_default(),
                            _ => t.trips.first().map(|tt| trip_of(&tt.trip).1).unwrap_or_default(),
                        };
                        let what = if end.is_empty() { format!("{} {}", tr("Tour"), t.number.trim()) } else { format!("{} {}  ›  {}", tr("Tour"), t.number.trim(), end) };
                        let when = match next {
                            Some(n) => hm((n.3 / 60.0) as f32),
                            None => t.trips.first().map(|tt| hm(tt.departure)).unwrap_or_default(),
                        };
                        (what, when)
                    })
                    .collect();
                Some(Preview { title: format!("{} {}", tr("Line"), line.name), meta: format!("{} {}", line.tours.iter().filter(|t| schedule.is_some_and(|s| tour_listed(s, &line.name, t, now))).count(), tr("tours")), rows, chosen: None, button: None, time: None })
            });
            (MenuKind::Lines, head("Line and tour..."), preview)
        }
        ListKind::Tours(line_name, pick) => {
            // (the line number of the trip of a tour shown: a tour may run trips of several lines)
            let tour_line = |ln: &str, num: &str| -> Option<String> {
                let sch = schedule?;
                let line = sch.data.lines.iter().find(|l| l.name == ln)?;
                let tour = line.tours.iter().find(|t| t.number == num)?;
                let n_trips = sch.tour_trip_count(ln, num);
                let trip = pick.as_ref().filter(|p| p.0 == num).map(|p| p.2).unwrap_or_else(|| sch.tour_trip_now(ln, num, now)).min(n_trips.saturating_sub(1));
                let stops = sch.tour_trip_stops(ln, num, trip);
                stops.first().and_then(|s| tour_trip_name(sch, tour, s.0)).map(|n| trip_of(&n).0).filter(|l| !l.is_empty())
            };
            let preview = action.strip_prefix("tour ").and_then(|rest| rest.split_once('\u{1}')).and_then(|(ln, num)| {
                let sch = schedule?;
                let line = sch.data.lines.iter().find(|l| l.name == ln)?;
                let tour = line.tours.iter().find(|t| t.number == num)?;
                let n_trips = sch.tour_trip_count(ln, num);
                let trip = pick.as_ref().filter(|p| p.0 == num).map(|p| p.2).unwrap_or_else(|| sch.tour_trip_now(ln, num, now)).min(n_trips.saturating_sub(1));
                let stops = sch.tour_trip_stops(ln, num, trip);
                let at = stops.first().map(|s| s.3).unwrap_or_else(|| tour_start(tour).unwrap_or(0.0));
                let chosen = pick.as_ref().filter(|p| p.0 == num).map(|p| p.1).unwrap_or(0).min(stops.len().saturating_sub(1));
                let rows = stops.iter().map(|s| (s.2.trim().to_string(), hm((s.3 / 60.0) as f32))).collect();
                let trip_line = tour_line(ln, num).unwrap_or_else(|| line_sign(schedule, line));
                Some(Preview {
                    title: format!("{} {}", tr("Tour"), num.trim()),
                    meta: format!("{} {}  ·  {} {}/{}  ·  {}", tr("Line"), trip_line, tr("Trip"), trip + 1, n_trips.max(1), tr("Choose the stop to start from")),
                    rows,
                    chosen: Some(chosen),
                    button: Some(tr("Start trip")),
                    // (the time of the trip: between the arrows that step through the tour's trips)
                    time: Some(hm((at / 60.0) as f32)),
                })
            });
            let chosen_line = action.strip_prefix("tour ").and_then(|rest| rest.split_once('\u{1}')).and_then(|(ln, num)| tour_line(ln, num));
            let sign = chosen_line.or_else(|| schedule.and_then(|s| s.data.lines.iter().find(|l| l.name == *line_name)).map(|l| line_sign(schedule, l))).unwrap_or_else(|| line_name.clone());
            (MenuKind::Tours, Some((title("Line and tour..."), format!("{} {}", tr("Line"), sign))), preview)
        }
        ListKind::Drivers => (MenuKind::List, head("Driver..."), None),
        ListKind::Numbers => (MenuKind::List, head("Fleet number..."), None),
        ListKind::Destinations => (MenuKind::List, head("Destination display..."), None),
        ListKind::RouteNumbers => (MenuKind::List, Some((tr("Route number"), String::new())), None),
        ListKind::Hofs => (MenuKind::List, head("Depot file (HOF)..."), None),
        ListKind::Spots => (MenuKind::List, head("Teleport to a start point..."), None),
        ListKind::PlaceMaker | ListKind::PlaceType(_) | ListKind::PlaceLivery(_) | ListKind::PlaceHof(..) => (MenuKind::List, head("Place a vehicle..."), None),
        ListKind::Admin => (MenuKind::List, Some((tr("Administration"), String::new())), None),
    }
}

/// Do a line of the list; returns the list to show next (None: back to the menu).
pub(crate) fn run(app: &mut App, kind: &ListKind, action: &str) -> Option<ListKind> {
    run_move(app, kind, action, Move::Next)
}

/// Do a line of the list, a setting changed as `mv` says (Enter and a click on a line are
/// `Move::Next`; the arrows and a click on a slider or a stepper the others).
pub(crate) fn run_move(app: &mut App, kind: &ListKind, action: &str, mv: Move) -> Option<ListKind> {
    if action == "back" {
        return match kind {
            ListKind::Tours(..) => Some(ListKind::Lines),
            ListKind::PlaceType(_) | ListKind::PlaceLivery(_) | ListKind::PlaceHof(..) => Some(ListKind::PlaceMaker),
            _ => None,
        };
    }
    let (verb, arg) = action.split_once(' ').unwrap_or((action, ""));
    match kind {
        ListKind::Admin => {
            crate::admin::run(app, action);
            Some(ListKind::Admin)
        }
        ListKind::Options(_) | ListKind::World(_) => {
            if verb == "noop" || option_do(app, verb, arg, mv) {
                return Some(kind.clone());
            }
            // (the arrows and a click on a stepper only change values: no buttons)
            let step = matches!(mv, Move::Next);
            match verb {
                "vr_nav_edit" if step && app.vr_active() && app.player.is_some() => {
                    app.start_vr_nav_edit();
                    return None;
                }
                "vr_nav_reset" if step && app.vr_active() && app.player.is_some() => {
                    app.vr_nav_adjust("reset", 1.0);
                    LIST_DIRTY.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                // (the preset, the clouds and the precipitation are picked from a drop-down: `App::chooser_pick`)
                "weather" | "cloudkind" | "precipkind" | "metar_src" | "sel" | "preset" | "gfxprofile" | "reset" => {}
                "metar_icao_edit" if step => {
                    if app.menu_edit_icao { app.apply_icao_edit(); } else { app.start_icao_edit(); }
                }
                // the exact time: Enter starts typing it, and sets it when typed
                "time_edit" if step => {
                    if app.menu_edit.is_some() {
                        app.apply_time_edit();
                    } else if app.lan.as_ref().is_some_and(|l| l.role == omsi_net::Role::Client) {
                        app.service_msg = Some(("In a LAN session the host sets the clock".into(), 3.0));
                    } else if app.real_time_locked() {
                        app.service_msg = Some(("The time cannot be changed while the real-time sync is on".into(), 3.0));
                    } else {
                        app.menu_edit = Some(String::new());
                    }
                }
                "seat_reset" if step => {
                    app.settings.seat = [0.0; 3];
                    for k in ["seat_x", "seat_y", "seat_z"] {
                        remember_setting(k, "0");
                    }
                }
                "clock_ontime" if step => {
                    if app.lan.as_ref().is_some_and(|l| l.role == omsi_net::Role::Client) {
                        app.service_msg = Some(("In a LAN session the host sets the clock".into(), 3.0));
                    } else if let Some(d) = app.player.as_ref().map(|p| p.vehicle.host.tt_delay as f64).filter(|d| d.abs() >= 1.0) {
                        // (the delay as it is now, not as the button was drawn: a second click
                        // would otherwise move the clock by the old amount again)
                        app.shift_clock(-d);
                        if let Some(p) = app.player.as_mut() {
                            p.vehicle.host.tt_delay = 0.0;
                        }
                    }
                }
                "clock_set" | "clock_shift" if step => {
                    if app.lan.as_ref().is_some_and(|l| l.role == omsi_net::Role::Client) {
                        app.service_msg = Some(("In a LAN session the host sets the clock".into(), 3.0));
                    } else if let Ok(secs) = arg.trim().parse::<f64>() {
                        let by = if verb == "clock_set" { secs - app.clock.time } else { secs };
                        app.shift_clock(by);
                    }
                }
                other if step => {
                    app.page_action(other);
                    return None;
                }
                _ => {}
            }
            Some(kind.clone())
        }
        ListKind::Vehicle(_) => {
            if matches!(mv, Move::Next) {
                app.page_action(verb);
                return None;
            }
            Some(kind.clone())
        }
        ListKind::Lines => match verb {
            "line" => {
                Some(ListKind::Tours(arg.to_string(), None))
            }
            "free" => {
                app.duty = None;
                // unscheduled: the GetTT* callbacks answer ""/0/-1 again, as in Omsi.exe
                if let Some(p) = app.player.as_mut() {
                    let h = &mut p.vehicle.host;
                    h.tt_line.clear();
                    h.tt_stops.clear();
                    h.tt_stop_ids.clear();
                    h.tt_busstop_index = -1;
                    h.tt_terminus_index = -1;
                    h.tt_delay = 0.0;
                }
                app.service_msg = Some(("Free drive: no duty".into(), 4.0));
                None
            }
            _ => None,
        },
        ListKind::Tours(_, pick) => {
            if let Some((line, tour)) = arg.split_once('\u{1}') {
                let chosen = pick.as_ref().filter(|p| p.0 == tour).map(|p| p.1).unwrap_or(0);
                let trip = pick.as_ref().filter(|p| p.0 == tour).map(|p| p.2).unwrap_or_else(|| app.schedule.as_ref().map(|s| s.tour_trip_now(line, tour, app.clock.time)).unwrap_or(0));
                start_duty_at(app, line, tour, trip, chosen);
            }
            None
        }
        ListKind::Drivers => {
            switch_driver(app, arg);
            Some(ListKind::Drivers)
        }
        ListKind::Hofs => {
            if let Some(p) = app.player.as_mut() {
                match omsi_vehicle::Hof::load(std::path::Path::new(arg)) {
                    Ok(h) => {
                        let name = h.name.clone();
                        p.vehicle.host.hof = Some(std::sync::Arc::new(h));
                        app.service_msg = Some((format!("Depot file: {}", name.trim()), 3.0));
                    }
                    Err(e) => app.service_msg = Some((format!("Depot file: {e}"), 4.0)),
                }
            }
            None
        }
        ListKind::Spots => {
            if app.lan.as_ref().is_some_and(|l| l.role == omsi_net::Role::Client) {
                app.service_msg = Some(("In a LAN session only the host moves vehicles on the map".into(), 4.0));
                return None;
            }
            let found = app.world.clone().and_then(|w| {
                let ep = arg.trim().parse::<usize>().ok().and_then(|i| w.global.entry_points.get(i))?;
                // (the entry points of tiles that are not loaded come from the map index)
                w.index();
                w.entry_point_place(ep).map(|(pos, rot)| (pos, rot[0]))
            });
            match found {
                Some((pos, heading)) => {
                    crate::admin::teleport(app, pos, heading);
                    app.service_msg = Some(("The bus stands at the start point".into(), 3.0));
                }
                None => app.service_msg = Some(("That start point is not in the map".into(), 3.0)),
            }
            None
        }
        ListKind::PlaceMaker | ListKind::PlaceType(_) if verb == "maker" => Some(ListKind::PlaceType(arg.to_string())),
        ListKind::PlaceMaker | ListKind::PlaceType(_) => Some(ListKind::PlaceLivery(arg.to_string())),
        ListKind::PlaceLivery(bus) => Some(ListKind::PlaceHof(bus.clone(), arg.to_string())),
        ListKind::PlaceHof(bus, paint) => {
            let (bus, paint, hof) = (bus.clone(), paint.clone(), arg.trim().to_string());
            app.close_game_menu();
            app.place_vehicle(&bus, Some(paint).filter(|p| !p.is_empty()), Some(hof).filter(|h| !h.is_empty()));
            None
        }
        ListKind::Destinations if verb == "routes" => Some(ListKind::RouteNumbers),
        ListKind::RouteNumbers if verb == "route_type" => {
            // the first press starts typing, the next one (Enter) sets what is typed
            match app.menu_edit.take() {
                Some(t) => {
                    set_route_by_hand(app, &t);
                    None
                }
                None => {
                    app.menu_edit = Some(String::new());
                    Some(ListKind::RouteNumbers)
                }
            }
        }
        ListKind::RouteNumbers => {
            set_route_by_hand(app, arg);
            None
        }
        ListKind::Destinations => {
            if let Some(p) = app.player.as_mut() {
                let hof = p.vehicle.host.hof.clone();
                let code: i32 = arg.trim().parse().unwrap_or(-1);
                if let Some(t) = hof.as_ref().and_then(|h| h.termini.iter().find(|t| t.code == code)) {
                    // (the line on the IBIS stays; only the destination changes)
                    let line = p.vehicle.var("IBIS_LinieKurs").filter(|l| *l > 0.0).map(|l| format!("{}", l as i64)).unwrap_or_default();
                    let name = t.strings.first().cloned().unwrap_or_default();
                    crate::schedule::set_player_destination_directly(&mut p.vehicle, hof.as_deref(), &line, &name, &[]);
                    log::info!("destination display set by hand: {code} {} (terminus code now {:?})", name.trim(), p.vehicle.var("IBIS_TerminusCode"));
                    app.service_msg = Some((format!("Destination: {}", name.trim()), 3.0));
                }
            }
            None
        }
        ListKind::Numbers => {
            if let (Some((n, reg)), Some(p)) = (arg.split_once('\u{1}'), app.player.as_mut()) {
                let v = &mut p.vehicle;
                if let Some(i) = v.ty.program.str_var("number") {
                    v.state.str_vars[i as usize] = n.to_string();
                }
                if !reg.is_empty() {
                    if let Some(i) = v.ty.program.str_var("ident") {
                        v.state.str_vars[i as usize] = reg.to_string();
                    }
                }
                app.service_msg = Some((format!("Fleet number {n}"), 3.0));
            }
            None
        }
    }
}

// ---------------------------------------------------------------------------------------
// the settings windows (options, vehicle, world): pages of rows

/// How a row's value is changed.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Move {
    /// Enter or a click on the line: the next value (a switch flips, a row of values wraps).
    Next,
    /// Left: the previous value, or off.
    Dec,
    /// Right: the next value, or on.
    Inc,
    /// A click on a slider: this far (0 to 1) along it.
    To(f32),
}

/// One page of a settings window: its title and its rows (label, action).
type Page = (&'static str, Vec<(String, String)>);

/// A row of a settings window (see `ui::MenuKind::Options` for the format).
fn row(name: &str, kind: char, value: &str, desc: &str, frac: Option<f32>) -> String {
    format!("{name}\u{1f}{kind}\u{1f}{value}\u{1f}{desc}\u{1f}{}", frac.map(|f| format!("{f:.3}")).unwrap_or_default())
}

/// A row that opens another list.
fn opens(name: &str, desc: &str, id: &str) -> (String, String) {
    (row(name, 'o', "", desc, None), id.to_string())
}

/// A row with a button that does something.
fn button(name: &str, text: &str, desc: &str, id: &str) -> (String, String) {
    (row(name, 'a', text, desc, None), id.to_string())
}

/// A switch row, if the setting `id` is one.
fn switch_row(app: &App, id: &str, name: &str, desc: &str) -> Option<(String, String)> {
    let on = toggle_now(app, id)?;
    Some((row(name, 's', if on { "on" } else { "off" }, desc, None), id.to_string()))
}

/// A slider row for the setting `id` ("verb" or "verb arg"); `fmt` writes its value.
fn slider_row(app: &App, id: &str, name: &str, desc: &str, fmt: &dyn Fn(f32) -> String) -> Option<(String, String)> {
    let (verb, arg) = id.split_once(' ').unwrap_or((id, ""));
    let steps = steps_of(verb)?;
    let now = option_now(app, verb, arg)?;
    let i = nearest(&steps, now);
    let frac = if steps.len() > 1 { i as f32 / (steps.len() - 1) as f32 } else { 0.0 };
    Some((row(name, 'v', &fmt(now), desc, Some(frac)), id.to_string()))
}

/// The values a slider's setting runs through.
fn steps_of(verb: &str) -> Option<Vec<f32>> {
    Some(match verb {
        "vr_nav_x" | "vr_nav_y" | "vr_nav_z" => (-100..=100).map(|v| v as f32 * 0.02).collect(),
        "triple_width_mm" => (20..=200).map(|v| v as f32 * 10.0).collect(),
        "triple_distance_mm" => (20..=300).map(|v| v as f32 * 10.0).collect(),
        "triple_bezel_mm" => (0..=100).map(|v| v as f32).collect(),
        "triple_left_angle_deg" | "triple_right_angle_deg" => (0..=90).map(|v| v as f32).collect(),
        "triple_eye_height_mm" => (-100..=100).map(|v| v as f32 * 5.0).collect(),
        "vr_nav_width" => (12..=65).map(|v| v as f32 * 0.01).collect(),
        "vr_nav_yaw" | "vr_nav_roll" => (-90..=90).map(|v| v as f32 * 2.0).collect(),
        "vr_nav_tilt" => (-40..=40).map(|v| v as f32 * 2.0).collect(),
        "vr_nav_opacity" => (6..=20).map(|v| v as f32 * 0.05).collect(),
        "speed" => SPEEDS.iter().map(|&v| v as f32).collect(),
        "traffic" => TRAFFIC.iter().map(|&v| v as f32).collect(),
        "pax" => PAX.to_vec(),
        "volume" => VOLUME.to_vec(),
        "led_glow" => (0..16).map(|v| v as f32).collect(),
        "led_mips" => (0..=80).map(|v| v as f32 * 0.05).collect(),
        "ui_scale" => (10..=40).map(|v| v as f32 * 0.05).collect(),
        "ui_opacity" => (4..=20).map(|v| v as f32 * 0.05).collect(),
        "vol_ai" | "vol_scenery" => (0..=20).map(|v| v as f32 * 0.05).collect(),
        "wheel_range" => (6..=60).map(|v| v as f32 * 30.0).collect(),
        "wheel_lock" => std::iter::once(0.0).chain((2..=60).map(|v| v as f32 * 30.0)).collect(),
        "fov" => std::iter::once(0.0).chain((20..=120).map(|v| v as f32)).collect(),
        "steer_look_angle" => (0..=60).map(|v| v as f32).collect(),
        "steer_look_response" => (1..=20).map(|v| v as f32 * 0.05).collect(),
        "head_idle" => (0..=20).map(|v| v as f32 * 0.05).collect(),
        "head_idle_pace" => (10..=40).map(|v| v as f32 * 0.05).collect(),
        "pedal_t" | "pedal_b" => PEDAL.to_vec(),
        "mouse_sens" => (10..=300).map(|v| v as f32 / 100.0).collect(),
        "look_sens" => (2..=40).map(|v| v as f32 * 0.05).collect(),
        "seat" => (-50..=50).map(|v| v as f32 / 100.0).collect(),
        "hour" => (0..24).map(|v| v as f32).collect(),
        "minute" => (0..60).map(|v| v as f32).collect(),
        // the weather, made by hand
        // (visibility goes by a few percent at a time from 100 m to 50 km, on whole tens of metres)
        "visibility" => {
            let mut v: Vec<f32> = (0..=120)
                .map(|i| {
                    let x = 100.0 * 500f32.powf(i as f32 / 120.0);
                    let step = if x < 1000.0 { 10.0 } else if x < 10000.0 { 100.0 } else { 500.0 };
                    (x / step).round() * step
                })
                .collect();
            v.dedup();
            v
        }
        "rain_amt" | "wet" => (0..=100).map(|v| v as f32 / 100.0).collect(),
        "brightness" => (0..=30).map(|v| v as f32 * 0.05).collect(),
        "humidity" => (0..=100).map(|v| v as f32).collect(),
        "temp" => (-20..=45).map(|v| v as f32).collect(),
        "wind_speed" => (0..=25).map(|v| v as f32).collect(),
        "wind_dir" => (0..360).map(|v| v as f32).collect(),
        _ => return None,
    })
}

/// The cloud types of OMSI's weather (`Weather/clouds.cfg`): the name in a weather file and
/// the name shown.
const CLOUD_TYPES: [(&str, &str); 5] = [("-1", "None"), ("Cumulus 1", "Few clouds"), ("Cumulus 2", "Scattered"), ("Cumulus 3", "Broken"), ("Overcast 1", "Overcast")];

/// The kinds of precipitation of a weather file (`[precip]`'s first number).
const PRECIP_KINDS: [&str; 3] = ["None", "Rain", "Snow"];

/// The name the weather has once it was set by hand.
pub(crate) const CUSTOM_WEATHER: &str = "Custom weather";

/// The index of the cloud type `kind` (a weather file's) in `CLOUD_TYPES`.
fn cloud_index(kind: &str) -> Option<usize> {
    let k = kind.trim();
    CLOUD_TYPES.iter().position(|(id, _)| id.eq_ignore_ascii_case(k) || (*id == "-1" && (k.is_empty() || k.starts_with("-1"))))
}

/// Set the kind of precipitation (an index of `PRECIP_KINDS`) of the weather set by hand.
fn custom_state(app:&App)->crate::weather_setup::CustomWeather{
    if let Some(mut c)=crate::weather_setup::custom_weather(app.args.weather.as_deref()){
        // Wetness keeps evolving while driving; never restore an old serialized value just
        // because another custom field (brightness, humidity, etc.) was edited.
        c.road_wetness=app.wetness;
        return c
    }
    match app.weather.as_ref(){
        Some(w)=>crate::weather_setup::CustomWeather::from_weather(w,1.0,app.wetness),
        None=>crate::weather_setup::CustomWeather::default(),
    }
}

pub(crate) fn set_precip(app: &mut App, to: usize) {
    let to = to.min(PRECIP_KINDS.len() - 1);
    app.edit_weather(|w| {
        w.precip[0] = to as f32;
        w.snow = to == 2;
        // (rain or snow with no strength would be nothing: a moderate one)
        if to != 0 && w.precip[1] < 1.0 {
            w.precip[1] = 100.0;
        }
    });
}

/// Whether `verb` is a slider (a click on its track sets the value there).
pub(crate) fn is_slider(verb: &str) -> bool {
    steps_of(verb).is_some()
}

/// The index of the value in `steps` nearest to `now`.
fn nearest(steps: &[f32], now: f32) -> usize {
    steps.iter().enumerate().min_by(|a, b| (a.1 - now).abs().total_cmp(&(b.1 - now).abs())).map(|x| x.0).unwrap_or(0)
}

/// The value of `steps` after moving from `now`.
fn step_move(steps: &[f32], now: f32, mv: Move) -> f32 {
    let n = steps.len().max(1);
    let i = nearest(steps, now);
    let to = match mv {
        Move::Next => (i + 1) % n,
        Move::Inc => (i + 1).min(n - 1),
        Move::Dec => i.saturating_sub(1),
        Move::To(f) => (f.clamp(0.0, 1.0) * (n - 1) as f32).round() as usize,
    };
    steps.get(to).copied().unwrap_or(now)
}

/// Keep the physical triple-screen calibration separate from the single-screen FOV.
fn set_camera_fov(settings: &mut crate::settings::Settings, value: f32) -> (&'static str, String) {
    let value = if value < 20.0 {
        0.0
    } else {
        value.round().min(120.0)
    };
    if settings.triple.enabled && !settings.vr_requested() {
        settings.triple.fov_deg = value;
        ("triple_fov_deg", value.to_string())
    } else {
        settings.fov = value;
        ("fov", value.to_string())
    }
}

/// The value of the slider setting `verb` (`arg`: the seat's axis).
fn option_now(app: &App, verb: &str, arg: &str) -> Option<f32> {
    if let Some(field) = verb.strip_prefix("vr_nav_") {
        return if app.vr_active() && app.player.is_some() { app.vr_nav_profile().value(field) } else { None };
    }
    let s = &app.settings;
    Some(match verb {
        "speed" => s.time_speed as f32,
        "traffic" => app.traffic.as_ref()?.target as f32,
        "pax" => s.pax_density,
        "volume" => s.volume,
        "led_glow" => s.led_glow as f32,
        "led_mips" => s.led_mips,
        "pedal_t" => s.pedal_throttle,
        "pedal_b" => s.pedal_brake,
        "mouse_sens" => s.mouse_sens,
        "look_sens" => s.look_sens,
        "ui_scale" => s.ui_scale,
        "ui_opacity" => s.ui_opacity,
        "vol_ai" => s.vol_ai,
        "vol_scenery" => s.vol_scenery,
        "wheel_range" => s.wheel_range,
        "wheel_lock" => s.wheel_lock,
        "triple_width_mm" => s.triple.width_mm,
        "triple_distance_mm" => s.triple.distance_mm,
        "triple_bezel_mm" => s.triple.bezel_mm,
        "triple_left_angle_deg" => s.triple.left_angle_deg,
        "triple_right_angle_deg" => s.triple.right_angle_deg,
        "triple_eye_height_mm" => s.triple.eye_height_mm,
        "fov" => {
            if s.triple.enabled && !s.vr_requested() {
                s.triple.fov_deg
            } else {
                s.fov
            }
        }
        "steer_look_angle" => s.steer_look_angle,
        "steer_look_response" => s.steer_look_response,
        "head_idle" => s.head_idle,
        "head_idle_pace" => s.head_idle_pace,
        "seat" => s.seat[arg.trim().parse::<usize>().unwrap_or(0).min(2)],
        "hour" => ((app.clock.time / 3600.0) as i64).rem_euclid(24) as f32,
        "minute" => (((app.clock.time / 60.0) as i64) % 60) as f32,
        "visibility" => app.weather.as_ref()?.fog.0,
        "rain_amt" => {
            let w = app.weather.as_ref()?;
            if w.precip.first().copied().unwrap_or(0.0) < 0.5 { 0.0 } else { (w.precip.get(1).copied().unwrap_or(0.0) / 255.0).clamp(0.0, 1.0) }
        }
        "wet" => app.wetness,
        "brightness" => custom_state(app).brightness,
        "humidity" => custom_state(app).humidity,
        "temp" => app.weather.as_ref()?.temp.0,
        "wind_speed" => app.weather.as_ref()?.wind.1,
        "wind_dir" => app.weather.as_ref()?.wind.0.rem_euclid(360.0),
        _ => return None,
    })
}

/// Set the slider setting `verb` to `v`; the key and value to keep for the next game.
fn option_set(app: &mut App, verb: &str, arg: &str, v: f32) -> Option<(&'static str, String)> {
    if let Some(field) = verb.strip_prefix("vr_nav_") {
        app.vr_nav_set(field, v);
        return None; // Stored per bus, never in the desktop settings file.
    }
    match verb {
        "speed" => {
            app.settings.time_speed = v as f64;
            Some(("time_speed", app.settings.time_speed.to_string()))
        }
        "traffic" => {
            if let Some(t) = app.traffic.as_mut() {
                t.target = v.round() as usize;
                app.args.traffic = t.target;
            }
            None
        }
        "pax" => {
            app.settings.pax_density = v;
            Some(("pax_density", v.to_string()))
        }
        "volume" => {
            app.settings.volume = v;
            Some(("volume", v.to_string()))
        }
        "led_glow" => {
            app.settings.led_glow = v.round() as _;
            Some(("led_glow", app.settings.led_glow.to_string()))
        }
        "led_mips" => {
            app.settings.led_mips = v.clamp(0.0, 4.0);
            Some(("led_mips", app.settings.led_mips.to_string()))
        }
        "pedal_t" => {
            app.settings.pedal_throttle = v;
            Some(("pedal_throttle", v.to_string()))
        }
        "pedal_b" => {
            app.settings.pedal_brake = v;
            Some(("pedal_brake", v.to_string()))
        }
        "look_sens" => {
            app.settings.look_sens = (v * 100.0).round() / 100.0;
            Some(("look_sens", app.settings.look_sens.to_string()))
        }
        "mouse_sens" => {
            app.settings.mouse_sens = (v * 100.0).round() / 100.0;
            Some(("mouse_sens", app.settings.mouse_sens.to_string()))
        }
        "ui_scale" => {
            app.settings.ui_scale = (v * 100.0).round() / 100.0;
            Some(("ui_scale", app.settings.ui_scale.to_string()))
        }
        "ui_opacity" => {
            app.settings.ui_opacity = (v * 100.0).round() / 100.0;
            Some(("ui_opacity", app.settings.ui_opacity.to_string()))
        }
        "vol_ai" => {
            app.settings.vol_ai = (v * 100.0).round() / 100.0;
            Some(("vol_ai", app.settings.vol_ai.to_string()))
        }
        "vol_scenery" => {
            app.settings.vol_scenery = (v * 100.0).round() / 100.0;
            Some(("vol_scenery", app.settings.vol_scenery.to_string()))
        }
        "wheel_range" => {
            app.settings.wheel_range = v.round();
            Some(("wheel_range", app.settings.wheel_range.to_string()))
        }
        "wheel_lock" => {
            app.settings.wheel_lock = if v < 45.0 { 0.0 } else { v.round() };
            Some(("wheel_lock", app.settings.wheel_lock.to_string()))
        }
        "triple_width_mm" => {
            app.settings.triple.width_mm = v.clamp(200.0, 2000.0);
            Some(("triple_width_mm", app.settings.triple.width_mm.to_string()))
        }
        "triple_distance_mm" => {
            app.settings.triple.fov_deg = 0.0;
            remember_setting("triple_fov_deg", "0");
            app.settings.triple.distance_mm = v.clamp(200.0, 3000.0);
            Some((
                "triple_distance_mm",
                app.settings.triple.distance_mm.to_string(),
            ))
        }
        "triple_bezel_mm" => {
            app.settings.triple.bezel_mm = v.clamp(0.0, 100.0);
            Some(("triple_bezel_mm", app.settings.triple.bezel_mm.to_string()))
        }
        "triple_left_angle_deg" => {
            app.settings.triple.left_angle_deg = v.clamp(0.0, 90.0);
            Some((
                "triple_left_angle_deg",
                app.settings.triple.left_angle_deg.to_string(),
            ))
        }
        "triple_right_angle_deg" => {
            app.settings.triple.right_angle_deg = v.clamp(0.0, 90.0);
            Some((
                "triple_right_angle_deg",
                app.settings.triple.right_angle_deg.to_string(),
            ))
        }
        "triple_eye_height_mm" => {
            app.settings.triple.eye_height_mm = v.clamp(-500.0, 500.0);
            Some((
                "triple_eye_height_mm",
                app.settings.triple.eye_height_mm.to_string(),
            ))
        }
        "fov" => Some(set_camera_fov(&mut app.settings, v)),
        "steer_look_angle" => {
            app.settings.steer_look_angle = v.round();
            Some(("steer_look_angle", app.settings.steer_look_angle.to_string()))
        }
        "steer_look_response" => {
            app.settings.steer_look_response = (v * 100.0).round() / 100.0;
            Some(("steer_look_response", app.settings.steer_look_response.to_string()))
        }
        "head_idle" => {
            app.settings.head_idle = (v * 100.0).round() / 100.0;
            Some(("head_idle", app.settings.head_idle.to_string()))
        }
        "head_idle_pace" => {
            app.settings.head_idle_pace = (v * 100.0).round() / 100.0;
            Some(("head_idle_pace", app.settings.head_idle_pace.to_string()))
        }
        "seat" => {
            let k: usize = arg.trim().parse().unwrap_or(0).min(2);
            app.settings.seat[k] = (v * 100.0).round() / 100.0;
            Some((["seat_x", "seat_y", "seat_z"][k], app.settings.seat[k].to_string()))
        }
        // the clock set directly: the hour or the minute (the seconds stay)
        "hour" | "minute" => {
            if app.lan.as_ref().is_some_and(|l| l.role == omsi_net::Role::Client) {
                app.service_msg = Some(("In a LAN session the host sets the clock".into(), 3.0));
                return None;
            }
            let t = app.clock.time;
            let (h, m) = (((t / 3600.0) as i64).rem_euclid(24), ((t / 60.0) as i64) % 60);
            let (h, m) = if verb == "hour" { (v.round() as i64, m) } else { (h, v.round() as i64) };
            let target = (h * 3600 + m * 60) as f64 + t % 60.0;
            app.shift_clock(target - t);
            None
        }
        // the weather, made by hand (what the preset was stays as it was but for this)
        "visibility" => {
            app.edit_weather(|w| w.fog.0 = v);
            None
        }
        "rain_amt" => {
            app.edit_weather(|w| {
                w.precip[1] = (v * 255.0).round();
                if v > 0.0 && w.precip[0] < 0.5 {
                    w.precip[0] = 1.0;
                }
            });
            None
        }
        "wet" => {
            let mut c=custom_state(app); c.road_wetness=v; app.set_custom_weather(c); None
        }
        "brightness" => {
            let mut c=custom_state(app); c.brightness=v; app.set_custom_weather(c); None
        }
        "humidity" => {
            let mut c=custom_state(app); c.humidity=v; app.set_custom_weather(c); None
        }
        "temp" => {
            app.edit_weather(|w| w.temp.0 = v);
            None
        }
        "wind_speed" => {
            app.edit_weather(|w| w.wind.1 = v);
            None
        }
        "wind_dir" => {
            app.edit_weather(|w| w.wind.0 = v);
            None
        }
        _ => None,
    }
}

/// Whether the switch `id` is on (None: `id` is no switch).
fn toggle_now(app: &App, id: &str) -> Option<bool> {
    let s = &app.settings;
    Some(match id {
        "navigator" => if app.vr_active() { app.vr_nav_profile().enabled } else { app.navigator.as_ref().is_some_and(|n| n.enabled) },
        "nav_ai" => app.navigator.as_ref().map_or(s.nav_ai, |n| n.show_ai),
        "shadows" => s.shadows,
        "head" => s.head_movement,
        "cam_smooth" => s.driverview_smooth,
        "coll_objects" => s.collision_objects,
        "coll_vehicles" => s.collision_vehicles,
        "mouse" => app.mouse_drive,
        "mouse_right" => s.mouse_right_off,
        "blinker_cancel" => s.blinker_cancel,
        "fps" => s.show_fps,
        "get_up" => s.get_up,
        "time_sync" => s.time_sync,
        "metar_sync" => s.metar_sync,
        "snow_cover" => app.weather.as_ref().is_some_and(|w|w.snow),
        "snow_road" => app.weather.as_ref().is_some_and(|w|w.snow_on_road),
        "camcoll" => s.camera_collision,
        "steer_look" => s.steer_look,
        "hands_in_cab" => s.hands_in_cab,
        "ff" => s.ff_enabled,
        "brake_hold" => s.brake_hold,
        "auto_clutch" => s.auto_clutch,
        "headtrack" => s.head_tracking,
        "timetable_win" => app.timetable,
        "info_bar" => app.info_bar,
        "nav_arrows" => app.navigator.as_ref().map_or(s.nav_arrows, |n| n.arrows),
        "exact_fare" => s.exact_fare,
        "collision_pedestrians" => s.collision_pedestrians,
        "ssao" => s.ssao,
        "detail_textures" => s.detail_textures,
        "reflections" => s.reflections,
        "clouds" => s.clouds,
        "fullscreen" => s.fullscreen,
        "vsync" => s.vsync,
        "texture_compression" => s.texture_compression,
        "driver" => s.driver,
        "alt_view" => s.alt_view,
        "triple_screen" => s.triple.enabled,
        "triple_hud_center" => s.triple_hud_center,
        "triple_span" => s.triple_span,
        "vr" => s.vr,
        "vr_desktop_mirror" => s.vr_desktop_mirror,
        "doppler" => s.doppler,
        "steering_linear" => s.steering_linear,
        "old_steering" => s.old_steering,
        "red_steer_spd" => s.red_steer_spd,
        "momentary_gears" => s.momentary_gears,
        "auto_shift" => s.auto_shift,
        "ff_invert" => s.ff_invert,
        "machine_translation" => s.machine_translation,
        "ui_scale_window" => s.ui_scale_window,
        "tooltips" => s.tooltips,
        "notes" => s.notes,
        "chat" => s.chat,
        "name_tags" => s.name_tags,
        _ => return None,
    })
}

/// Switch `id` on or off; the key and value to keep for the next game.
fn toggle_set(app: &mut App, id: &str, on: bool) -> Option<(&'static str, String)> {
    let bit = (on as u8).to_string();
    match id {
        "navigator" => {
            if app.vr_active() {
                if app.vr_nav_profile().enabled != on { app.vr_nav_adjust("enabled", 1.0); }
                return None;
            }
            if let Some(n) = app.navigator.as_mut() {
                n.enabled = on;
            }
            app.settings.navigator = on;
            Some(("navigator", bit))
        }
        "nav_ai" => {
            if let Some(n) = app.navigator.as_mut() {
                n.show_ai = on;
            }
            app.settings.nav_ai = on;
            Some(("nav_ai", bit))
        }
        "shadows" => {
            app.settings.shadows = on;
            Some(("shadows", bit))
        }
        "head" => {
            app.settings.head_movement = on;
            Some(("head_movement", bit))
        }
        "cam_smooth" => {
            app.settings.driverview_smooth = on;
            Some(("driverview_smooth", bit))
        }
        // (at once: stuck under a bridge a map made too low, the bus drives on)
        "coll_objects" => {
            app.settings.collision_objects = on;
            let cw = app.world.as_ref().map(|w| w.collision.lock().clone());
            if let Some(p) = app.player.as_mut() {
                p.vehicle.collision = cw.filter(|_| on);
            }
            Some(("collision_objects", bit))
        }
        "coll_vehicles" => {
            app.settings.collision_vehicles = on;
            Some(("collision_vehicles", bit))
        }
        "mouse" => {
            // (as the O key does it: switched off from the menu, the brake the mouse held
            // stayed behind and the bus rolled away - #517, #760)
            app.set_mouse_drive(on);
            None
        }
        "blinker_cancel" => {
            app.settings.blinker_cancel = on;
            if let Some(p) = app.player.as_mut() {
                p.blinker_cancel = on;
            }
            Some(("blinker_cancel", bit))
        }
        "mouse_right" => {
            app.settings.mouse_right_off = on;
            Some(("mouse_right_off", bit))
        }
        "get_up" => {
            app.settings.get_up = on;
            Some(("get_up", bit))
        }
        // the real-time sync: the clock takes the device's date and time at once (a host's
        // clock runs at real time while it is on, at its time speed again after)
        "time_sync" => {
            app.settings.time_sync = on;
            if let Some(l) = app.lan.as_mut().filter(|l| l.role == omsi_net::Role::Host) {
                l.clock_speed = if on { 1.0 } else { app.settings.time_speed.clamp(1.0, 30.0) };
            }
            app.sync_real_time();
            Some(("time_sync", bit))
        }
        // the METAR sync: the weather goes over to the report of the nearest airport and
        // cannot be changed while it is on (the cycle and a hand-made weather end with it)
        "metar_sync" => {
            app.settings.metar_sync = on;
            app.metar_rx = None;
            app.metar_once = false;
            app.metar_next = 0.0;
            if on {
                app.weather_cycle = None;
                app.weather_blend = None;
            }
            Some(("metar_sync", bit))
        }
        "snow_cover" => { app.edit_weather(|w|w.snow=on); None }
        "snow_road" => { app.edit_weather(|w|w.snow_on_road=on); None }
        "fps" => {
            app.settings.show_fps = on;
            Some(("show_fps", bit))
        }
        "headtrack" => {
            app.settings.head_tracking = on;
            Some(("head_tracking", bit))
        }
        "camcoll" => {
            app.settings.camera_collision = on;
            Some(("camera_collision", bit))
        }
        "steer_look" => {
            app.settings.steer_look = on;
            Some(("steer_look", bit))
        }
        "hands_in_cab" => {
            app.settings.hands_in_cab = on;
            Some(("hands_in_cab", bit))
        }
        "brake_hold" => {
            app.settings.brake_hold = on;
            Some(("brake_hold", bit))
        }
        "auto_clutch" => {
            app.settings.auto_clutch = on;
            if let Some(p) = app.player.as_mut() {
                p.vehicle.host.auto_clutch = if on { 1.0 } else { 0.0 };
            }
            Some(("auto_clutch", bit))
        }
        "ff" => {
            app.settings.ff_enabled = on;
            Some(("ff_enabled", bit))
        }
        "timetable_win" => {
            app.timetable = on;
            None
        }
        "info_bar" => {
            app.info_bar = on;
            None
        }
        "nav_arrows" => {
            app.settings.nav_arrows = on;
            Some(("nav_arrows", bit))
        }
        "exact_fare" => {
            app.settings.exact_fare = on;
            Some(("exact_fare", bit))
        }
        "collision_pedestrians" => {
            app.settings.collision_pedestrians = on;
            Some(("collision_pedestrians", bit))
        }
        "ssao" => {
            app.settings.ssao = on;
            Some(("ssao", bit))
        }
        "detail_textures" => {
            app.settings.detail_textures = on;
            Some(("detail_textures", bit))
        }
        "reflections" => {
            app.settings.reflections = on;
            Some(("reflections", bit))
        }
        "clouds" => {
            app.settings.clouds = on;
            Some(("clouds", bit))
        }
        "fullscreen" => {
            app.settings.fullscreen = on;
            if app.spanned {
                log::info!("triple screen: the window spans three monitors, fullscreen is left alone");
            } else if let Some(w) = app.window.as_ref() {
                w.set_fullscreen(on.then_some(winit::window::Fullscreen::Borderless(None)));
            }
            Some(("fullscreen", bit))
        }
        "vsync" => {
            app.settings.vsync = on;
            Some(("vsync", bit))
        }
        "texture_compression" => {
            app.settings.texture_compression = on;
            Some(("texture_compression", bit))
        }
        "driver" => {
            app.settings.driver = on;
            Some(("driver", bit))
        }
        "alt_view" => {
            app.settings.alt_view = on;
            Some(("alt_view", bit))
        }
        "triple_screen" => {
            app.settings.triple.enabled = on;
            Some(("triple_screen", bit))
        }
        "triple_hud_center" => {
            app.settings.triple_hud_center = on;
            Some(("triple_hud_center", bit))
        }
        "triple_span" => {
            app.settings.triple_span = on;
            Some(("triple_span", bit))
        }
        "vr" => {
            app.settings.vr = on;
            Some(("vr", bit))
        }
        "vr_desktop_mirror" => {
            app.settings.vr_desktop_mirror = on;
            Some(("vr_desktop_mirror", bit))
        }
        "doppler" => {
            app.settings.doppler = on;
            Some(("doppler", bit))
        }
        "steering_linear" => {
            app.settings.steering_linear = on;
            Some(("steering_linear", bit))
        }
        "old_steering" => {
            app.settings.old_steering = on;
            Some(("old_steering", bit))
        }
        "red_steer_spd" => {
            app.settings.red_steer_spd = on;
            Some(("red_steer_spd", bit))
        }
        "momentary_gears" => {
            app.settings.momentary_gears = on;
            // (the bus being driven read it when it was taken over)
            if let Some(p) = app.player.as_mut() {
                p.momentary_gears = on;
            }
            Some(("momentary_gears", bit))
        }
        "auto_shift" => {
            app.settings.auto_shift = on;
            if let Some(p) = app.player.as_mut() {
                p.auto_shift = on;
            }
            Some(("auto_shift", bit))
        }
        "ff_invert" => {
            app.settings.ff_invert = on;
            Some(("ff_invert", bit))
        }
        "machine_translation" => {
            app.settings.machine_translation = on;
            crate::mt::enable(on);
            Some(("machine_translation", bit))
        }
        "ui_scale_window" => {
            app.settings.ui_scale_window = on;
            Some(("ui_scale_window", bit))
        }
        "tooltips" => {
            app.settings.tooltips = on;
            Some(("tooltips", bit))
        }
        "notes" => {
            app.settings.notes = on;
            Some(("notes", bit))
        }
        "chat" => {
            app.settings.chat = on;
            Some(("chat", bit))
        }
        "name_tags" => {
            app.settings.name_tags = on;
            Some(("name_tags", bit))
        }
        _ => None,
    }
}

/// Set by `option_do` when a value really changed (the open list is out of date then).
pub(crate) static LIST_DIRTY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Change the setting `verb` (a switch or a slider) as `mv` says; false when it is neither.
fn option_do(app: &mut App, verb: &str, arg: &str, mv: Move) -> bool {
    // (the weather is the METAR report's while the sync is on)
    if app.metar_locked() && matches!(verb, "visibility" | "rain_amt" | "wet" | "brightness" | "humidity" | "temp" | "wind_speed" | "wind_dir" | "snow_cover" | "snow_road") {
        app.service_msg = Some(("The weather cannot be changed while the METAR sync is on".into(), 3.0));
        return true;
    }
    if let Some(cur) = toggle_now(app, verb) {
        let on = match mv {
            Move::Next => !cur,
            Move::Inc => true,
            Move::Dec => false,
            Move::To(f) => f >= 0.5,
        };
        if on != cur {
            if let Some((k, v)) = toggle_set(app, verb, on) {
                remember_setting(k, &v);
            }
            sync_live(app);
            LIST_DIRTY.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        return true;
    }
    if let Some(steps) = steps_of(verb) {
        if let Some(now) = option_now(app, verb, arg) {
            let to = step_move(&steps, now, mv);
            // (a slider dragged sends the same value many times over)
            if (to - now).abs() > 1e-6 {
                if let Some((k, v)) = option_set(app, verb, arg, to) {
                    remember_setting(k, &v);
                }
                sync_live(app);
                LIST_DIRTY.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        return true;
    }
    false
}

/// The name of the weather in force (the file's name without its ending).
/// The weather files (`Weather/*.owt`) of every content root, by name.
fn weather_files() -> Vec<String> {
    let mut files: Vec<String> = omsi_cfg::read_dir_merged("Weather")
        .into_iter()
        .filter(|p| p.extension().map(|e| e.eq_ignore_ascii_case("owt")).unwrap_or(false))
        .filter_map(|p| p.file_name().map(|n| format!("Weather/{}", n.to_string_lossy())))
        .collect();
    files.sort_by(|a, b| bus_cmp(a.trim_start_matches("Weather/").trim_start_matches('#'), b.trim_start_matches("Weather/").trim_start_matches('#')));
    files.dedup();
    files
}

/// A drop-down over a row of a settings window (the weather preset, the clouds), as a
/// select in a page: the entries drop down under the row's value.
pub(crate) struct Dropdown {
    /// The row of the window it belongs to.
    pub row: usize,
    /// (label, action) of its entries.
    pub items: Vec<(String, String)>,
    /// The entry the keyboard is on, the first one shown, and the one in force now.
    pub sel: usize,
    pub top: usize,
    pub current: Option<usize>,
}

/// The drop-down of the row `row` whose action is `id`, if that row has one.
pub(crate) fn dropdown_for(app: &App, row: usize, id: &str) -> Option<Dropdown> {
    let tr = |t: &str| omsi_ui::tr(t).into_owned();
    if app.metar_locked() && matches!(id, "weather" | "cloudkind" | "precipkind") {
        return None;
    }
    let mut current: Option<usize> = None;
    let items: Vec<(String, String)> = match id {
        "weather" => {
            let now = weather_name(app);
            weather_files()
                .into_iter()
                .enumerate()
                .map(|(i, f)| {
                    let stem = f.rsplit('/').next().unwrap_or(&f).rsplit_once('.').map(|x| x.0).unwrap_or(&f).trim_start_matches('#').to_string();
                    if stem.eq_ignore_ascii_case(&now) {
                        current = Some(i);
                    }
                    (stem, format!("wx {f}"))
                })
                .collect()
        }
        "metar_src" => {
            let mut v = vec![(tr("Automatic (nearest the map)"), "metar_src ".to_string())];
            let own = &app.settings.metar_station;
            current = Some(0);
            for (i, (code, label)) in crate::weather_setup::metar_airports(&app.args.root).into_iter().enumerate() {
                if code.eq_ignore_ascii_case(own) {
                    current = Some(i + 1);
                }
                v.push((label, format!("metar_src {code}")));
            }
            v
        }
        "cloudkind" => {
            current = app.weather.as_ref().and_then(|w| cloud_index(&w.clouds.0));
            CLOUD_TYPES.iter().enumerate().map(|(i, (_, n))| (tr(*n), format!("cloud {i}"))).collect()
        }
        "precipkind" => {
            current = app.weather.as_ref().map(|w| (w.precip.first().copied().unwrap_or(0.0).max(0.0) as usize).min(PRECIP_KINDS.len() - 1));
            PRECIP_KINDS.iter().enumerate().map(|(i, n)| (tr(*n), format!("precip {i}"))).collect()
        }
        "preset" => {
            current = preset_now(&settings_file());
            presets().iter().enumerate().map(|(i, p)| (tr(p.0), format!("preset {i}"))).collect()
        }
        "gfxprofile" => omsi_launcher_lib::graphics_profiles().into_keys().map(|n| (n.clone(), format!("gfxprofile {n}"))).collect(),
        "reset" => vec![(tr("Cancel"), "noop".to_string()), (tr("Reset all settings"), "reset_all".to_string())],
        key if key.starts_with("sel ") => {
            let key = &key[4..];
            let (options, at, _) = select_state(&settings_file(), key);
            current = at;
            options.iter().map(|o| (tr(o.1), format!("pick {key} {}", o.0))).collect()
        }
        _ => return None,
    };
    if items.is_empty() {
        return None;
    }
    let sel = current.unwrap_or(0);
    Some(Dropdown { row, items, sel, top: 0, current })
}

/// Do an entry of a drop-down.
pub(crate) fn dropdown_apply(app: &mut App, action: &str) {
    let (verb, arg) = action.split_once(' ').unwrap_or((action, ""));
    if app.metar_locked() && matches!(verb, "wx" | "cloud" | "precip") {
        app.service_msg = Some(("The weather cannot be changed while the METAR sync is on".into(), 3.0));
        return;
    }
    match verb {
        "wx" => {
            if app.lan.as_ref().is_some_and(|l| l.role == omsi_net::Role::Client) {
                app.service_msg = Some(("In a LAN session the host sets the weather".into(), 3.0));
            } else {
                app.change_weather(Some(arg.to_string()), true, 1.0);
            }
        }
        "metar_src" => {
            let code: String = arg.trim().chars().filter(|c| c.is_ascii_alphabetic()).take(4).collect::<String>().to_ascii_uppercase();
            app.settings.metar_station = code.clone();
            remember_setting("metar_station", &code);
            app.metar_rx = None;
            app.metar_once = false;
            // With sync on, the new station is fetched at once. With it off this simply
            // selects the station for "Load current METAR once".
            app.metar_next = 0.0;
        }
        "cloud" => {
            if let Some(i) = arg.trim().parse::<usize>().ok().filter(|i| *i < CLOUD_TYPES.len()) {
                app.edit_weather(|w| {
                    w.clouds.0 = CLOUD_TYPES[i].0.to_string();
                    if i == 0 {
                        w.clouds.1 = 0.0;
                    }
                });
            }
        }
        "precip" => {
            if let Some(i) = arg.trim().parse::<usize>().ok().filter(|i| *i < PRECIP_KINDS.len()) {
                set_precip(app, i);
            }
        }
        "pick" => {
            if let Some((key, value)) = arg.split_once(' ') {
                remember_setting(key, value);
                reload_settings(app);
            }
        }
        "preset" => {
            if let Some(p) = arg.trim().parse::<usize>().ok().and_then(|i| presets().into_iter().nth(i)) {
                store_with(app, |v| {
                    if let Some(o) = p.1.as_object() {
                        for (k, x) in o {
                            v[k.as_str()] = x.clone();
                        }
                    }
                });
            }
        }
        "gfxprofile" => {
            let name = arg.trim();
            match omsi_launcher_lib::graphics_profiles().get(name) {
                Some(p) => {
                    store_with(app, |v| omsi_launcher_lib::apply_graphics_profile(p, v));
                    sync_live(app);
                    LIST_DIRTY.store(true, std::sync::atomic::Ordering::Relaxed);
                    app.service_msg = Some((format!("Graphics profile \"{name}\" loaded: graphics settings apply when the game starts the next time"), 5.0));
                }
                None => app.service_msg = Some((format!("Graphics profile \"{name}\" not found"), 4.0)),
            }
        }
        "reset_all" => store_with(app, |v| {
            let language = v.get("language").cloned();
            *v = omsi_launcher_lib::settings_from_text(None);
            if let Some(l) = language {
                v["language"] = l;
            }
        }),
        _ => {}
    }
}

fn weather_name(app: &App) -> String {
    if app.weather.as_ref().is_some_and(|w| w.name == CUSTOM_WEATHER) {
        return CUSTOM_WEATHER.to_string();
    }
    // a METAR report's weather
    if app.args.weather.as_deref().is_some_and(|p| p.starts_with(crate::weather_setup::REPORT)) {
        if let Some(n) = app.weather.as_ref().map(|w| w.name.trim().to_string()).filter(|n| !n.is_empty()) {
            return n;
        }
    }
    match app.args.weather.as_deref() {
        Some(p) => {
            let p = p.replace('\\', "/");
            let file = p.rsplit('/').next().unwrap_or("");
            let stem = file.rsplit_once('.').map(|x| x.0).unwrap_or(file);
            stem.trim_start_matches('#').to_string()
        }
        None => app.weather.as_ref().map(|w| w.name.trim().to_string()).filter(|n| !n.is_empty()).unwrap_or_else(|| "Map default".to_string()),
    }
}

/// The launcher's settings file as the lists show it: read once (until something is
/// written), with the keys still waiting to be written on top.
fn settings_file() -> serde_json::Value {
    let mut v = SETTINGS_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(|| {
            let text = std::fs::read_to_string(omsi_launcher_lib::data_dir().join("settings.cfg")).ok();
            omsi_launcher_lib::settings_from_text(text.as_deref())
        })
        .clone();
    let pending = PENDING_SETTINGS.lock().unwrap_or_else(|e| e.into_inner());
    apply_pending(&mut v, &pending.0);
    v
}

fn value_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(x) => x.clone(),
        serde_json::Value::Bool(b) => (*b as u8).to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

fn same_value(a: &str, b: &str) -> bool {
    a == b || a.parse::<f64>().ok().zip(b.parse::<f64>().ok()).is_some_and(|(x, y)| (x - y).abs() < 1e-6)
}

fn select_options(key: &str) -> Vec<(&'static str, &'static str)> {
    match key {
        "graphics" => vec![("vanilla", "Vanilla (as OMSI 2)"), ("vanilla_plus", "Vanilla+"), ("enhanced", "Enhanced")],
        "msaa" => vec![("1", "Off"), ("2", "2x MSAA"), ("4", "4x MSAA"), ("8", "8x MSAA")],
        "render_scale" => vec![("auto", "Auto"), ("1", "100%"), ("0.85", "85%"), ("0.75", "75%"), ("0.67", "67%"), ("0.5", "50%")],
        "anisotropy" => vec![("1", "Off"), ("2", "2x"), ("4", "4x"), ("8", "8x")],
        "shadow_size" => vec![("1024", "1024"), ("2048", "2048"), ("4096", "4096")],
        "shadow_casters" => vec![("all", "Every solid mesh"), ("omsi", "[shadow] meshes, as OMSI")],
        "max_fps" => vec![("0", "Screen refresh rate"), ("30", "30 fps"), ("45", "45 fps"), ("60", "60 fps"), ("120", "120 fps"), ("144", "144 fps"), ("1000", "Unlimited")],
        "view_distance" => vec![("auto", "Default (1200 m)"), ("600", "600 m - fastest"), ("900", "900 m"), ("1200", "1200 m"), ("1500", "1500 m"), ("2000", "2000 m"), ("2500", "2500 m")],
        "max_obj_dist" => vec![("auto", "Automatic"), ("500", "500 m"), ("750", "750 m"), ("900", "900 m"), ("1500", "1500 m"), ("3000", "3000 m")],
        "min_obj_size" => vec![("0.005", "All"), ("0.013", "Normal"), ("0.02", "Fewer (faster)"), ("0.03", "Few (fastest)")],
        "mirror_size" => vec![("0", "Off"), ("128", "Low (128)"), ("256", "Normal (256)"), ("512", "High (512)"), ("1024", "Very high (1024)")],
        "texture_memory" => vec![("0", "Automatic"), ("500", "500 MB"), ("1000", "1 GB"), ("1500", "1.5 GB"), ("2000", "2 GB"), ("3000", "3 GB"), ("4000", "4 GB"), ("6000", "6 GB")],
        "drive_keys" => vec![("omsi", "Custom controls (Controls page)"), ("simple", "W A S D + arrows"), ("wasd", "W A S D only"), ("arrows", "Arrow keys only")],
        "resolution" => crate::launcher::pages::RESOLUTIONS.to_vec(),
        "navigator_corner" => vec![("top-left", "Top left"), ("top-right", "Top right"), ("bottom-left", "Bottom left"), ("bottom-right", "Bottom right")],
        "boarding" => vec![("auto", "Pay and take the ticket"), ("pay", "The driver sells the ticket"), ("walk", "Just walk in")],
        "pax_voices" => vec![("all", "Greetings and tickets"), ("tickets", "Only the ticket asked for"), ("off", "Silent")],
        "maintenance" => vec![("0", "Infinite (no wear)"), ("1", "Very bad"), ("2", "Bad"), ("3", "Normal"), ("4", "Good")],
        "ai_unsched_factor" => vec![("25", "25%"), ("50", "50%"), ("75", "75%"), ("100", "100%"), ("150", "150%"), ("200", "200%")],
        "ai_max_scheduled" => vec![("0", "All"), ("10", "At most 10"), ("25", "At most 25"), ("50", "At most 50")],
        "ai_max_parked" => vec![("-1", "None"), ("0", "Every space"), ("35", "At most 35"), ("100", "At most 100"), ("250", "At most 250")],
        "language" => omsi_launcher_lib::LANGUAGES.iter().map(|l| (l.0, l.1)).collect(),
        "vr_scale" => vec![("0.5", "50%"), ("0.65", "65%"), ("0.8", "80%"), ("1", "100%")],
        "vr_head_smoothing_ms" => vec![("0", "Off"), ("5", "5 ms"), ("10", "10 ms"), ("20", "20 ms"), ("30", "30 ms")],
        "vr_mirror_rate" => vec![("0", "Off"), ("8", "8/s"), ("16", "16/s"), ("24", "24/s"), ("32", "32/s"), ("48", "48/s"), ("60", "60/s"), ("90", "90/s"), ("120", "120/s"), ("180", "180/s"), ("240", "240/s"), ("360", "360/s"), ("-1", "Every frame")],
        _ => Vec::new(),
    }
}

fn select_state(file: &serde_json::Value, key: &str) -> (Vec<(&'static str, &'static str)>, Option<usize>, String) {
    let options = select_options(key);
    let cur = value_text(file.get(key).unwrap_or(&serde_json::Value::Null));
    let at = options.iter().position(|o| same_value(o.0, &cur));
    (options, at, cur)
}

fn select_row(file: &serde_json::Value, key: &str, name: &str, desc: &str) -> Option<(String, String)> {
    let (options, at, cur) = select_state(file, key);
    if options.is_empty() {
        return None;
    }
    let label = at.map(|i| omsi_ui::tr(options[i].1).into_owned()).unwrap_or(cur);
    Some((row(name, 'o', &label, desc, None), format!("sel {key}")))
}

fn presets() -> [(&'static str, serde_json::Value); 4] {
    [
        ("Low", serde_json::json!({"msaa": 1, "anisotropy": 2, "shadow_size": 1024, "ssao": false, "shadows": false, "detail_textures": false, "clouds": false, "view_distance": "600", "min_obj_size": 0.03, "max_obj_dist": "500", "mirror_size": 128, "render_scale": "0.75", "texture_memory": 800})),
        ("Medium", serde_json::json!({"msaa": 2, "anisotropy": 4, "shadow_size": 2048, "ssao": false, "shadows": true, "detail_textures": true, "clouds": true, "view_distance": "900", "min_obj_size": 0.02, "max_obj_dist": "750", "mirror_size": 256, "render_scale": "auto", "texture_memory": 1200})),
        ("High", serde_json::json!({"msaa": 4, "anisotropy": 8, "shadow_size": 2048, "ssao": true, "shadows": true, "detail_textures": true, "clouds": true, "view_distance": "auto", "min_obj_size": 0.013, "max_obj_dist": "auto", "mirror_size": 256, "render_scale": "auto", "texture_memory": 0})),
        ("Ultra", serde_json::json!({"msaa": 4, "anisotropy": 8, "shadow_size": 4096, "ssao": true, "shadows": true, "detail_textures": true, "clouds": true, "view_distance": "2000", "min_obj_size": 0.005, "max_obj_dist": "1500", "mirror_size": 512, "render_scale": "auto", "texture_memory": 0})),
    ]
}

fn preset_now(file: &serde_json::Value) -> Option<usize> {
    presets().iter().position(|p| {
        p.1.as_object().is_some_and(|o| o.iter().all(|(k, v)| same_value(&value_text(v), &value_text(file.get(k).unwrap_or(&serde_json::Value::Null)))))
    })
}

fn preset_row(file: &serde_json::Value, name: &str, desc: &str) -> Option<(String, String)> {
    let label = omsi_ui::tr(preset_now(file).map(|i| presets()[i].0).unwrap_or("Custom")).into_owned();
    Some((row(name, 'o', &label, desc, None), "preset".to_string()))
}

fn store_with(app: &mut App, change: impl FnOnce(&mut serde_json::Value)) {
    flush_settings(true);
    let Ok(mut v) = omsi_launcher_lib::get_settings() else { return };
    change(&mut v);
    *SETTINGS_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    match omsi_launcher_lib::save_settings(&v) {
        Ok(()) => reload_settings(app),
        Err(e) => log::warn!("settings not saved: {e:#}"),
    }
}

fn reload_settings(app: &mut App) {
    flush_settings(true);
    app.settings = crate::settings::Settings::load();
    crate::ui_language(&app.settings.language);
    sync_live(app);
}

fn sync_live(app: &mut App) {
    let s = &app.settings;
    crate::startup::SOUND_AI.store(s.vol_ai.to_bits(), std::sync::atomic::Ordering::Relaxed);
    crate::startup::SOUND_SCENERY.store(s.vol_scenery.to_bits(), std::sync::atomic::Ordering::Relaxed);
    omsi_audio::DOPPLER.store(s.doppler, std::sync::atomic::Ordering::Relaxed);
    if let Some(n) = app.navigator.as_mut() {
        n.arrows = s.nav_arrows;
    }
    if let Some(h) = app.humans.as_mut() {
        h.exact_fare = s.exact_fare;
        h.boarding = s.boarding.clone();
        h.voices = match s.pax_voices.as_str() {
            "off" => 2,
            "tickets" => 1,
            _ => 0,
        };
    }
}

fn options_pages(app: &App) -> Vec<Page> {
    let s = &app.settings;
    let file = settings_file();
    let pick = |key: &str, name: &str, desc: &str| select_row(&file, key, name, desc);
    let pct = |v: f32| format!("{:.0} %", v * 100.0);
    let cm = |v: f32| format!("{:+.0} cm", v * 100.0);
    let later = "Takes effect when the game starts the next time";
    let game: Vec<(String, String)> = vec![
        switch_row(app, "navigator", "Navigator", "Enables/Disables the Minimap"),
        switch_row(app, "nav_ai", "AI vehicles on the map", "Shows/hides the other (AI) vehicles on the Minimap and the city map"),
        switch_row(app, "nav_arrows", "Route arrows (as in OMSI 2)", "Shows OMSI 2's route arrows over the road"),
        pick("navigator_corner", "Corner", later),
        switch_row(app, "get_up", "Ability to get up (Ctrl+Shift+G)", "Allows you to get out of the car and explore the world"),
        switch_row(app, "coll_objects", "Collisions with objects", "Enables/disables collisions with objects such as buildings, streetlights, etc."),
        switch_row(app, "coll_vehicles", "Collisions with vehicles", "Enables/Disables Collisions with Other Vehicles"),
        switch_row(app, "collision_pedestrians", "Collisions with people", "Enables/disables knocking down people"),
        switch_row(app, "timetable_win", "Timetable window", "Displays a list of all stops (only when a tour is active)"),
        switch_row(app, "info_bar", "Information bar", "Displays information such as the time, speed, and other details at the top of the screen"),
        switch_row(app, "exact_fare", "Passengers pay the exact fare", "No change is given at the cash desk"),
        pick("boarding", "Boarding", "How passengers get their tickets"),
        pick("maintenance", "Maintenance", later),
        pick("ai_unsched_factor", "Random traffic", later),
        pick("ai_max_scheduled", "Timetable vehicles", later),
        pick("ai_max_parked", "Parked cars", later),
    ]
        .into_iter()
        .flatten()
        .collect();
    let graphics: Vec<(String, String)> = vec![
        preset_row(&file, "Quality preset", "Sets most of the graphics options at once"),
        pick("graphics", "Graphics", later),
        pick("msaa", "Anti-aliasing", later),
        pick("render_scale", "Render scale", later),
        pick("anisotropy", "Anisotropic", later),
        switch_row(app, "shadows", "Sun shadows", "Enables/Disabled shadows"),
        pick("shadow_size", "Shadow map", later),
        switch_row(app, "ssao", "Ambient occlusion", later),
        pick("shadow_casters", "Shadows cast by", later),
        switch_row(app, "detail_textures", "Detail texturing up close", "The ground and large walls get fine grain when close"),
        slider_row(app, "led_glow", "LED glow", "How strongly the dots of LED destination displays glow", &|v| format!("{}/15", v as i64)),
        slider_row(app, "led_mips", "LED mask mipmaps", "Keep the mip chain of the LED masks (smoother from a distance).", &|v| format!("{v:.2}")),
        switch_row(app, "reflections", "Reflection maps (paint, chrome, glass)", later),
        switch_row(app, "clouds", "Clouds", later),
    ]
        .into_iter()
        .flatten()
        .collect();
    let mut display = vec![
        switch_row(app, "fullscreen", "Fullscreen", "Switches the window between windowed and fullscreen"),
        pick("resolution", "Window size", later),
        switch_row(
            app,
            "triple_screen",
            "Triple screen",
            "Three physical screen projections; OpenXR takes priority",
        ),
    ];
    // (the rig's own settings only while it is on)
    let triple = vec![
        switch_row(
            app,
            "triple_hud_center",
            "HUD on centre screen",
            "Keep the navigator, menus and information on the centre screen",
        ),
        switch_row(
            app,
            "triple_span",
            "Span three monitors at startup",
            "Borderless across three equal monitors in one horizontal row; restart required",
        ),
        slider_row(
            app,
            "triple_width_mm",
            "Visible panel width",
            "Width of one screen without its frame",
            &|v| format!("{v:.0} mm"),
        ),
        slider_row(
            app,
            "triple_distance_mm",
            "Eye distance",
            "Eye to the centre screen",
            &|v| format!("{v:.0} mm"),
        ),
        slider_row(
            app,
            "triple_bezel_mm",
            "Frame width at each join",
            "Combined width of both adjacent frames",
            &|v| format!("{v:.0} mm"),
        ),
        slider_row(
            app,
            "triple_left_angle_deg",
            "Left screen angle",
            "Inward angle from a flat row",
            &|v| format!("{v:.0}°"),
        ),
        slider_row(
            app,
            "triple_right_angle_deg",
            "Right screen angle",
            "Inward angle from a flat row",
            &|v| format!("{v:.0}°"),
        ),
        slider_row(
            app,
            "triple_eye_height_mm",
            "Eye above screen centre",
            "Vertical eye offset",
            &|v| format!("{v:.0} mm"),
        ),
    ];
    if app.settings.triple.enabled {
        display.extend(triple);
    }
    let display: Vec<(String, String)> = display.into_iter().chain([
        switch_row(app, "vsync", "V-sync", "Waits for the screen's refresh"),
        pick("max_fps", "Frame limit", "Frames a second at most"),
        switch_row(app, "fps", "Frame rate", "Show the frames per second in the top right corner"),
        pick("view_distance", "View distance", later),
        pick("max_obj_dist", "Object distance", later),
        pick("min_obj_size", "Small objects", later),
        pick("mirror_size", "Mirrors", later),
        pick("texture_memory", "Texture memory", later),
        switch_row(app, "texture_compression", "Compress textures on loading", later),
        (!omsi_launcher_lib::graphics_profiles().is_empty()).then(|| opens("Load graphics profile", "Applies a graphics profile saved in the launcher", "gfxprofile")),
    ])
        .flatten()
        .collect();
    let sound: Vec<(String, String)> = vec![
        slider_row(app, "volume", "Volume", "Set how loud the game should be", &pct),
        slider_row(app, "vol_ai", "Traffic", "How loud the other vehicles are", &pct),
        slider_row(app, "vol_scenery", "Surroundings", "How loud the sounds of the scenery are", &pct),
        switch_row(app, "doppler", "Doppler effect", "Approaching sounds higher, receding ones lower"),
        pick("pax_voices", "Passenger voices", "What passengers say"),
    ]
        .into_iter()
        .flatten()
        .collect();
    let mut camera: Vec<(String, String)> = vec![
        switch_row(app, "head", "Head movement", "The view moves with the vehicle's acceleration"),
        switch_row(app, "cam_smooth", "Smooth viewpoint changes", "Enables a smooth transition between camera perspectives"),
        switch_row(app, "camcoll", "Camera collisions", "The outside camera cannot pass through objects"),
        switch_row(app, "steer_look", "View turns with steering", "Camera turns with the steering wheel (cockpit only)"),
        slider_row(app, "steer_look_angle", "Steering view angle", "How far the view turns at full steering lock", &|v| format!("{v:.0}°")),
        slider_row(app, "steer_look_response", "Steering view response", "How quickly the view follows the steering", &|v| format!("{:.0} ms", v * 1000.0)),
        slider_row(app, "head_idle", "Head sway at a standstill", "How much the view sways on its own when nothing is done to it - a head at rest is never quite still, most of it seen while the bus waits at a stop", &|v| if v <= 0.0 { "Off".to_string() } else { format!("{:.0}%", v * 100.0) }),
        slider_row(app, "head_idle_pace", "Sway pace", "How fast that sway moves (100% is the pace it is designed at)", &|v| format!("{:.0}%", v * 100.0)),
        switch_row(app, "hands_in_cab", "Driver's hands in the cab view", "Shows the driver's hand on the steering wheel (Cockpit only)"),
        switch_row(app, "driver", "Driver at the wheel (outside views)", "Shows the driver in the outside views and in the mirrors"),
        switch_row(app, "headtrack", "Head tracking", &format!("Head tracking with opentrack (UDP port {})", s.head_tracking_port)),
        slider_row(app, "look_sens", "Mouse look sensitivity", "How fast the view turns when looking round with the mouse (100% is OMSI's)", &pct),
        switch_row(app, "alt_view", "Right mouse button turns the view", "Shift+right zooms; off: right zooms as in OMSI, the wheel button turns"),
        slider_row(app, "fov", "Field of view", "Vertical field of view; in triple screen Default uses physical measurements, an override moves the virtual eye", &|v| if v < 20.0 { "Default".to_string() } else { format!("{v:.0}°") }),
        slider_row(app, "seat 1", "Seat forward and back", "Adjust the driver's seat position forward or backward", &cm),
        slider_row(app, "seat 2", "Seat height", "Adjust the driver's seat height", &cm),
        slider_row(app, "seat 0", "Seat left and right", "Adjust the driver's seat position from side to side", &cm),
    ]
        .into_iter()
        .flatten()
        .collect();
    camera.push(button("Reset the seat position", "Reset", "Put the seat back where the vehicle has it.", "seat_reset"));
    if cfg!(windows) {
        camera.extend(
            vec![
                switch_row(app, "vr", "Use OpenXR headset", later),
                if s.vr { pick("vr_scale", "Eye resolution", later) } else { None },
                if s.vr { pick("vr_head_smoothing_ms", "Head tracking smoothing", later) } else { None },
                if s.vr { pick("vr_mirror_rate", "Bus mirror refresh", later) } else { None },
                if s.vr { switch_row(app, "vr_desktop_mirror", "Show headset picture on monitor", later) } else { None },
            ]
                .into_iter()
                .flatten(),
        );
    }
    let controls: Vec<(String, String)> = vec![
        pick("drive_keys", "Driving keys", "Which keys drive the vehicle"),
        switch_row(app, "mouse", "Steering with the mouse", "Steer and control the pedals using the mouse"),
        switch_row(app, "mouse_right", "A right click ends the mouse steering", "As in OMSI; off: the right button only looks round"),
        slider_row(app, "mouse_sens", "Mouse steering sensitivity", "Adjust how much the steering wheel turns based on mouse movement", &pct),
        switch_row(app, "steering_linear", "Steering linearity (keys at OMSI's steady pace)", "Keyboard steering at OMSI's steady pace"),
        switch_row(app, "old_steering", "Old Steering (the wheel stays, turn it back yourself)", "The wheel stays where the keys left it"),
        switch_row(app, "red_steer_spd", "Dynamic steering (slower keys at speed, OMSI's redSteerSpd)", "The steering keys act slower at speed"),
        switch_row(app, "ff", "Force feedback and vibration", "Enable force feedback for the steering wheel and vibration for controllers"),
        switch_row(app, "ff_invert", "Invert force feedback by default", "For wheels without a saved direction"),
        slider_row(app, "wheel_range", "Wheel rotation", "The steering wheel's own rotation, lock to lock", &|v| format!("{v:.0}°")),
        slider_row(app, "wheel_lock", "Full lock at", "How far the wheel turns for the vehicle's full lock", &|v| if v < 45.0 { "OMSI".to_string() } else { format!("{v:.0}°") }),
        slider_row(app, "pedal_t", "Throttle pedal strength", "Adjust how strongly pedal input affects the throttle", &|v| format!("x{v}")),
        slider_row(app, "pedal_b", "Brake pedal strength", "Adjust how strongly pedal input affects the brake", &|v| format!("x{v}")),
        switch_row(app, "blinker_cancel", "Indicators cancel themselves", "The bus's script turns the indicator off after a turn; off: it stays on until you turn it off"),
        switch_row(app, "brake_hold", "Keyboard brake stays on", "Keep the brake applied until the throttle is pressed"),
        switch_row(app, "auto_clutch", "Automatic clutch", "Automatically operate the clutch for you"),
        switch_row(app, "momentary_gears", "Hold manual gear buttons (release returns to neutral)", later),
        switch_row(app, "auto_shift", "Automated manual gearbox", "Shift a manual gearbox's gears for you by the engine speed"),
    ]
        .into_iter()
        .flatten()
        .collect();
    let interface: Vec<(String, String)> = vec![
        pick("language", "Language", "The language of the game's interface"),
        switch_row(app, "machine_translation", "Translate the remaining texts automatically (offline, downloads 620 MB once)", "Translates texts nobody has translated, on this machine"),
        slider_row(app, "ui_scale", "Game interface size", "The size of the texts, the menu, the timetable and the navigator", &pct),
        switch_row(app, "ui_scale_window", "Interface grows with the window", "On a window taller than 1080p the interface grows with it"),
        slider_row(app, "ui_opacity", "Interface opacity", "How much of the interface's backgrounds shows", &pct),
        switch_row(app, "tooltips", "Name of the button under the mouse", "Shows the name of what the cursor points at"),
        switch_row(app, "notes", "Notes in the top-left corner", "Why the vehicle does not move, the change due, what a service did"),
        switch_row(app, "chat", "Chat in online games", "Shows the chat of a LAN session"),
        switch_row(app, "name_tags", "Other players' names above their buses", "Shows the names of the other players"),
        Some(opens("Reset all settings...", "Everything but the language, the key bindings and the game folder goes back to how it came", "reset")),
    ]
        .into_iter()
        .flatten()
        .collect();
    let mut pages = vec![("Gameplay", game), ("Graphics", graphics), ("Display and memory", display), ("Sound", sound), ("Camera", camera), ("Controls", controls), ("Interface", interface)];
    if app.vr_active() && app.player.is_some() {
        let desc = "Navigator position (this bus)";
        let mut rows = vec![
            switch_row(app, "navigator", "Navigator", desc).unwrap(),
            button("Move and rotate with the mouse...", "Open", desc, "vr_nav_edit"),
        ];
        for (id, label) in [("x", "Position right / left"), ("y", "Position forward / back"), ("z", "Position up / down"), ("width", "Display width")] {
            rows.extend(slider_row(app, &format!("vr_nav_{id}"), label, desc, &cm));
        }
        for (id, label) in [("yaw", "Display rotation"), ("tilt", "Display tilt"), ("roll", "Display roll")] {
            rows.extend(slider_row(app, &format!("vr_nav_{id}"), label, desc, &|v| format!("{v:.0}°")));
        }
        rows.extend(slider_row(app, "vr_nav_opacity", "Interface opacity", desc, &pct));
        rows.push(button("Reset navigator position", "Reset", desc, "vr_nav_reset"));
        pages.push(("VR", rows));
    }
    pages
}

fn vehicle_pages(app: &App) -> Vec<Page> {
    let has = app.player.is_some();
    let server = crate::input_script::on_server(&app.args);
    let mut display: Vec<(String, String)> = Vec::new();
    if has {
        display.push(opens("Destination display", "Change the current destination", "dest"));
        display.push(opens("Depot file (HOF)", "Change the current depot file (used for the timetable)", "hof"));
        display.push(opens("Fleet number", "Change the vehicle's current fleet number", "number"));
    }
    if !server {
        display.push(opens("Driver", "Change the current driver profile", "driver"));
    }
    let mut fleet: Vec<(String, String)> = Vec::new();
    if has || !app.placed.is_empty() {
        fleet.push(button("Drive the next vehicle", "Switch", "Take the wheel of another vehicle standing in the world", "switch"));
    }
    fleet.push(opens("Place a vehicle", "Place a vehicle of your choice", "place"));
    if has {
        fleet.push(button("Couple", "Couple", "Couple the vehicle to the one in front of or behind it", "couple"));
        fleet.push(button("Uncouple", "Uncouple", "Separate the coupled vehicles", "uncouple"));
        if app.on_foot.is_none() {
            fleet.push(button("Get up and out", "Get out", "Step out of your car and explore the world", "getout"));
        }
        fleet.push(button("Remove this vehicle", "Remove", "Removes the current vehicle", "remove"));
        // (#728: another bus in this one's place, or this one again with its files read
        // anew - a script or a .bus changed - without starting the game again)
        fleet.push(button("Swap for another vehicle", "Swap", "Put another vehicle in this one's place and drive it", "swap"));
        fleet.push(button("Reload this vehicle", "Reload", "Read the vehicle's files again (.bus, model and sound configuration, scripts) and drive it from here", "reload"));
    }
    if !app.placed.is_empty() {
        fleet.push(button("Remove the placed vehicles", "Remove", "Removes all vehicles you've placed from the world", "clearplaced"));
    }
    let mut service: Vec<(String, String)> = Vec::new();
    if has {
        service.push(button("Refuel", "Refuel", "Fills the tank of the current vehicle", "refuel"));
        service.push(button("Wash", "Wash", "Cleans the current vehicle", "wash"));
        service.push(button("Repair", "Repair", "Repairs the current vehicle", "repair"));
        service.push(button("Put back on its wheels", "Reset", "Return the vehicle to an upright position", "reset"));
        if !server && app.navigator.is_some() {
            service.push(button("Move on the map", "Pick", "Teleports you to any location on the map", "teleport"));
            service.push(opens("Teleport to a start point", "Teleport to a starting point on the map", "tplist"));
        }
    }
    vec![("Display and driver", display), ("Vehicles", fleet), ("Service", service)]
}

fn world_pages(app: &App) -> Vec<Page> {
    let client = app.lan.as_ref().is_some_and(|l| l.role == omsi_net::Role::Client);
    let pct = |v: f32| format!("{:.0} %", v * 100.0);
    let mut time: Vec<(String, String)> = Vec::new();
    let mut weather: Vec<(String, String)> = Vec::new();
    let mut climate: Vec<(String, String)> = Vec::new();
    let mut tools: Vec<(String, String)> = Vec::new();
    if !client {
        let t = app.clock.time;
        let now = format!("{:02}:{:02}", ((t / 3600.0) as i64).rem_euclid(24), ((t / 60.0) as i64) % 60);
        time.extend(switch_row(app, "time_sync", "Real-time sync", "The game follows your device's date and time"));
        if app.real_time_locked() {
            let (d, m) = app.clock.day_month();
            let text = format!("{:04}-{m:02}-{d:02}  {}:{:02}", app.clock.year, now, (t as i64) % 60);
            time.push((row("Date and time", 'i', &text, "Synchronized with the real time", None), "noop".to_string()));
        } else {
            // the exact time: typed as hours, minutes and seconds
            match app.menu_edit.as_ref() {
                Some(d) => {
                    let mut c: Vec<char> = d.chars().collect();
                    c.resize(6, '_');
                    let typed = format!("{}{}:{}{}:{}{}", c[0], c[1], c[2], c[3], c[4], c[5]);
                    time.push((row("Exact time", 'E', &typed, "Press Enter to change, Esc to cancel", None), "time_edit".to_string()));
                }
                None => {
                    let secs = format!("{}:{:02}", now, (t as i64) % 60);
                    time.push((row("Exact time", 'e', &secs, "Change the current time (Press Enter to change)", None), "time_edit".to_string()));
                }
            }
            time.extend(slider_row(app, "hour", "Hour", "Set the hour of the day directly", &|v| format!("{:02}", v as i64)));
            time.extend(slider_row(app, "minute", "Minute", "Set the minute directly", &|v| format!("{:02}", v as i64)));
            for (name, hm, secs) in [("Morning", "06:00", 6 * 3600), ("Noon", "12:00", 12 * 3600), ("Evening", "18:00", 18 * 3600), ("Night", "23:00", 23 * 3600)] {
                time.push(button(name, hm, "Jump to this time of day.", &format!("clock_set {secs}")));
            }
            if let (Some(_), Some(p)) = (app.duty.as_ref(), app.player.as_ref()) {
                let d = p.vehicle.host.tt_delay as f64;
                if d.abs() >= 1.0 {
                    let text = format!("{}{}:{:02}", if d < 0.0 { "−" } else { "+" }, (d.abs() / 60.0) as i64, d.abs() as i64 % 60);
                    time.push(button("On time with the timetable", &text, "Move the clock so that the vehicle is on time", "clock_ontime"));
                }
            }
            if app.lan.is_none() {
                time.extend(slider_row(app, "speed", "Time speed", "How fast the world's clock runs", &|v| format!("x{v}")));
            }
        }
        weather.extend(switch_row(app, "metar_sync", "METAR sync", "The weather follows the real METAR report"));
        let src = if app.settings.metar_station.is_empty() { format!("{} ({})", app.metar_station(), omsi_ui::tr("automatic")) } else { app.metar_station() };
        weather.push((row("METAR source", 'o', &src, "The airport used for real weather.", None), "metar_src".to_string()));
        let typed=if app.menu_edit_icao{
            let mut s=app.menu_edit.clone().unwrap_or_default(); while s.len()<4{s.push('_');} format!("{s}  (typing)")
        }else{app.metar_station()};
        weather.push((row("ICAO",if app.menu_edit_icao{'E'}else{'e'},&typed,"Enter any 4-letter ICAO station.",None),"metar_icao_edit".to_string()));
        if app.metar_locked() {
            weather.push(button("METAR report", "Refresh now", "Fetch the selected station again without waiting for the next automatic update.", "metar_refresh"));
        } else {
            weather.push(button("METAR report", "Load once", "Load the selected station once without enabling continuous METAR sync.", "metar_once"));
        }
        weather.push((row("Preset", 'o', &weather_name(app), "A ready-made weather", None), "weather".to_string()));
        if !app.metar_locked() {
            weather.push(button("Custom weather", "Edit current", "Freeze the weather currently in force and edit it as a custom weather.", "weather_custom"));
        }
        let cloud = app.weather.as_ref().and_then(|w| cloud_index(&w.clouds.0)).map(|i| CLOUD_TYPES[i].1.to_string()).or_else(|| app.weather.as_ref().map(|w| w.clouds.0.trim().to_string())).unwrap_or_default();
        weather.push((row("Clouds", 'o', &cloud, "The kind of clouds in the sky.", None), "cloudkind".to_string()));
        weather.extend(slider_row(app, "visibility", "Visibility", "How far one can see; less is fog.", &|v| if v >= 1000.0 { format!("{:.1} km", v / 1000.0) } else { format!("{} m", v as i64) }));
        weather.extend(slider_row(app,"brightness","Brightness","Brightness of the custom weather lighting.",&|v|format!("{:.0} %",v*100.0)));
        let kind = app.weather.as_ref().map(|w| (w.precip.first().copied().unwrap_or(0.0).max(0.0) as usize).min(PRECIP_KINDS.len() - 1)).unwrap_or(0);
        weather.push((row("Precipitation", 'o', PRECIP_KINDS[kind], "Rain or snow.", None), "precipkind".to_string()));
        weather.extend(slider_row(app, "rain_amt", "Precipitation strength", "How hard it rains or snows.", &pct));
        weather.extend(slider_row(app, "wet", "Wet roads", "How wet the roads are now (they dry in the sun, wet in the rain).", &pct));
        weather.extend(switch_row(app,"snow_cover","Snow cover","Snow lying on the world and ground."));
        weather.extend(switch_row(app,"snow_road","Snow on road","Treat the road surface as snow-covered."));
        climate.extend(slider_row(app, "temp", "Temperature", "The air temperature.", &|v| format!("{} °C", v as i64)));
        let dew_temp=app.weather.as_ref().map(|w|w.temp.0).unwrap_or(15.0);
        climate.extend(slider_row(app,"humidity","Humidity","Relative humidity of the air.",&|v|format!("{:.0} % · dew {:.0} °C",v,crate::weather_setup::dew_point_c(dew_temp,v))));
        climate.extend(slider_row(app, "wind_speed", "Wind speed", "How fast the wind blows; it drives the clouds.", &|v| format!("{} m/s", v as i64)));
        climate.extend(slider_row(app, "wind_dir", "Wind direction", "The direction of the wind in degrees (0 is north).", &|v| format!("{}°", v as i64)));
        // the METAR sync on: only its own rows stay (the weather is the report's)
        if app.metar_locked() {
            weather.retain(|r| matches!(r.1.as_str(), "metar_sync" | "metar_src" | "metar_icao_edit" | "metar_refresh"));
            climate.clear();
        }
        tools.push(button("Object editor", "Open", "Place and move objects in the world.", "editor"));
    }
    let mut people: Vec<(String, String)> = Vec::new();
    people.extend(slider_row(app, "traffic", "Traffic", "How many vehicles drive around the map.", &|v| format!("{} vehicles", v as i64)));
    people.extend(slider_row(app, "pax", "Passengers", "How many passengers wait at the stops and ride.", &pct));
    vec![("Time", time), ("Weather", weather), ("Temperature and wind", climate), ("Traffic and people", people), ("Tools", tools)]
}

/// The pages of the settings window `kind` (empty ones left out) and the one shown.
fn pages_of(app: &App, kind: &ListKind) -> Option<(Vec<Page>, usize)> {
    let (pages, tab) = match kind {
        ListKind::Options(t) => (options_pages(app), *t),
        ListKind::Vehicle(t) => (vehicle_pages(app), *t),
        ListKind::World(t) => (world_pages(app), *t),
        _ => return None,
    };
    let pages: Vec<Page> = pages.into_iter().filter(|p| !p.1.is_empty()).collect();
    let tab = tab.min(pages.len().saturating_sub(1));
    Some((pages, tab))
}

/// Which tab of the Options window is the one titled `title` (the first if none is).
pub(crate) fn options_tab(app: &App, title: &str) -> usize {
    pages_of(app, &ListKind::Options(0)).and_then(|(pages, _)| pages.iter().position(|p| p.0 == title)).unwrap_or(0)
}

type TitlesCache = Option<(ListKind, bool, std::time::Instant, (Vec<String>, usize))>;

thread_local! {
    static TITLES: std::cell::RefCell<TitlesCache> = const { std::cell::RefCell::new(None) };
}

pub(crate) fn forget_page_titles() {
    TITLES.with(|c| *c.borrow_mut() = None);
}

/// The titles of the pages of an open settings window and the one shown.
///
/// Asked every frame while a window is open, and building the pages is the work of
/// all their rows: the answer is kept for a moment.
pub(crate) fn page_titles(app: &App, kind: &ListKind) -> Option<(Vec<String>, usize)> {
    let vr_nav_available = app.vr_active() && app.player.is_some();
    if let Some(hit) = TITLES.with(|c| {
        c.borrow().as_ref().filter(|(k, vr, t, _)| k == kind && *vr == vr_nav_available && t.elapsed().as_millis() < 5000).map(|(_, _, _, r)| r.clone())
    }) {
        return Some(hit);
    }
    let (pages, tab) = pages_of(app, kind)?;
    let r = (pages.iter().map(|p| p.0.to_string()).collect::<Vec<_>>(), tab);
    TITLES.with(|c| *c.borrow_mut() = Some((kind.clone(), vr_nav_available, std::time::Instant::now(), r.clone())));
    Some(r)
}

/// The time (seconds of the day) a tour starts: its earliest trip's departure.
pub(crate) fn tour_start(tour: &omsi_timetable::Tour) -> Option<f64> {
    tour.trips.iter().map(|t| t.departure as f64 * 60.0).fold(None, |a: Option<f64>, d| Some(a.map_or(d, |x| x.min(d))))
}

/// Whether a tour is listed now: it runs on this day and is current at `now` (seconds of the
/// day) - under way, or leaving within half an hour; one that has finished is not.
fn tour_listed(sch: &crate::schedule::Schedule, line: &str, tour: &omsi_timetable::Tour, now: f64) -> bool {
    if !sch.tour_available(tour) {
        return false;
    }
    let start = tour_start(tour).unwrap_or(0.0);
    let end = sch.tour_stops(line, &tour.number).iter().map(|s| s.3).fold(start, f64::max);
    // (a night tour's times go on past 24:00: the early hours of the next day count too)
    [now, now + 86400.0].iter().any(|n| *n >= start - 1800.0 && *n <= end + 60.0)
}

/// The line number a line's trips carry (`.ttp` line), else the name of the line's file.
fn line_sign(schedule: Option<&crate::schedule::Schedule>, line: &omsi_timetable::Line) -> String {
    let sign = schedule.and_then(|sch| {
        line.tours.iter().flat_map(|t| t.trips.iter()).find_map(|tt| {
            let t = sch.data.trips.iter().find(|x| x.name.eq_ignore_ascii_case(&tt.trip))?;
            Some(t.line.trim().to_string()).filter(|n| !n.is_empty())
        })
    });
    sign.unwrap_or_else(|| line.name.clone())
}

/// A line's tours in alphabetical order of their numbers (numbers inside them as numbers:
/// "2" before "10"; equal numbers by the time they start).
fn sorted_tours(line: &omsi_timetable::Line) -> Vec<&omsi_timetable::Tour> {
    let mut tours: Vec<&omsi_timetable::Tour> = line.tours.iter().collect();
    tours.sort_by(|a, b| {
        bus_cmp(a.number.trim(), b.number.trim()).then_with(|| {
            let (ta, tb) = (tour_start(a).unwrap_or(f64::MAX), tour_start(b).unwrap_or(f64::MAX));
            ta.partial_cmp(&tb).unwrap_or(std::cmp::Ordering::Equal)
        })
    });
    tours
}

/// The name of trip number `k` of a tour, counted as `Schedule::tour_stops` does (trips the
/// timetable does not know are left out).
fn tour_trip_name(sch: &crate::schedule::Schedule, tour: &omsi_timetable::Tour, k: usize) -> Option<String> {
    tour.trips
        .iter()
        .filter(|tt| sch.data.trips.iter().any(|x| x.name.eq_ignore_ascii_case(&tt.trip)))
        .nth(k)
        .map(|tt| tt.trip.clone())
}

/// Numbers compared as numbers where they are ("5" before "13", "N30" after "M49").
fn natural(a: &str, b: &str) -> std::cmp::Ordering {
    let key = |s: &str| {
        let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
        (digits.parse::<u64>().unwrap_or(u64::MAX), s.to_ascii_lowercase())
    };
    key(a).cmp(&key(b))
}

static PENDING_SETTINGS: std::sync::Mutex<(Vec<(String, String)>, Option<std::time::Instant>)> =
    std::sync::Mutex::new((Vec::new(), None));
const SETTINGS_FLUSH_MS: u128 = 250;
static SETTINGS_CACHE: std::sync::Mutex<Option<serde_json::Value>> = std::sync::Mutex::new(None);

/// Write one key of `~/.openomsi/settings.cfg` (the launcher's file; the other lines
/// stay as they are). The write is delayed a moment and joined with the ones that follow.
pub(crate) fn remember_setting(key: &str, value: &str) {
    {
        let mut p = PENDING_SETTINGS.lock().unwrap_or_else(|e| e.into_inner());
        match p.0.iter_mut().find(|(k, _)| k == key) {
            Some(e) => e.1 = value.to_string(),
            None => p.0.push((key.to_string(), value.to_string())),
        }
    }
    flush_settings(false);
}

/// Write the remembered keys out: all of them when `force`, else only when the last write
/// is `SETTINGS_FLUSH_MS` ago. Called every frame, before the file is read and on exit.
pub(crate) fn flush_settings(force: bool) {
    let pending = {
        let mut p = PENDING_SETTINGS.lock().unwrap_or_else(|e| e.into_inner());
        if p.0.is_empty() {
            return;
        }
        if !force && p.1.is_some_and(|t| t.elapsed().as_millis() < SETTINGS_FLUSH_MS) {
            return;
        }
        p.1 = Some(std::time::Instant::now());
        std::mem::take(&mut p.0)
    };
    let Ok(mut v) = omsi_launcher_lib::get_settings() else { return };
    apply_pending(&mut v, &pending);
    if let Err(e) = omsi_launcher_lib::save_settings(&v) {
        log::warn!("settings not saved: {e:#}");
    }
    *SETTINGS_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn apply_pending(v: &mut serde_json::Value, pending: &[(String, String)]) {
    for (key, value) in pending {
        let parsed: serde_json::Value = value.parse::<f64>().map(serde_json::Value::from).unwrap_or_else(|_| serde_json::Value::from(value.as_str()));
        // a switch goes in as true/false, as the launcher's own values are: written as 1 it
        // was read as not set and saved back as its default (the pause menu's options were
        // lost with the next game)
        let parsed = match (&v[key.as_str()], &parsed) {
            (serde_json::Value::Bool(_), serde_json::Value::Number(n)) => serde_json::Value::Bool(n.as_f64().unwrap_or(0.0) > 0.5),
            _ if key == "time_speed" => serde_json::Value::from(value.as_str()),
            _ => parsed,
        };
        v[key.as_str()] = parsed;
    }
}

/// The personnel files there are (content folder and OMSI 2's `Drivers`), by name.
fn driver_names(app: &App) -> Vec<String> {
    let mut names: Vec<String> = omsi_cfg::read_dir_merged("Drivers")
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("odr")))
        .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
        .collect();
    let _ = app;
    names.sort_by_key(|n| n.to_ascii_lowercase());
    names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    names
}

/// Go on with another driver: this run so far into the old personnel file, the rest into
/// the new one.
fn switch_driver(app: &mut App, name: &str) {
    if app.career.path.is_some() {
        if let Err(e) = app.career.save() {
            log::warn!("writing the personnel file: {e}");
        }
    }
    let rel = format!("Drivers/{name}.odr");
    let mut next = crate::career::Career::load(&app.args.root, &rel);
    // (the distance and the clock of the run go on; the counters start with the new file)
    next.seconds = app.career.seconds;
    app.career = next;
    app.args.driver = Some(rel);
    app.service_msg = Some((format!("Driver: {name}"), 3.0));
}

/// The fleet numbers of the bus's `[number]` list with their registrations.
fn fleet_numbers(v: &omsi_sim::VehicleInstance) -> Vec<(String, String)> {
    let def = &v.ty.def;
    def.numbers_with_plates()
        .into_iter()
        .map(|(n, _)| {
            let reg = if def.registration_mode == 1 { String::new() } else { def.chosen_plate_of_number(&n) };
            (n, reg)
        })
        .collect()
}

/// Take on line `line`, tour `tour` from now: the duty, and the IBIS typed for it.
/// The tour on row `k` of the open list of tours: (line, tour).
pub(crate) fn tour_at(app: &App, k: usize) -> Option<(String, String)> {
    let action = app.admin_list.as_ref()?.get(k)?.1.strip_prefix("tour ")?;
    let (line, tour) = action.split_once('\u{1}')?;
    Some((line.to_string(), tour.to_string()))
}

/// The time (seconds of the day) tour `tour` of line `line` starts.
pub(crate) fn tour_start_of(app: &App, line: &str, tour: &str) -> f64 {
    app.schedule
        .as_ref()
        .and_then(|s| s.data.lines.iter().find(|l| l.name == line))
        .and_then(|l| l.tours.iter().find(|t| t.number == tour))
        .and_then(tour_start)
        .unwrap_or(0.0)
}

/// For the tour on row `k`: how many stops its chosen trip has, the stop chosen to start
/// from, the trip chosen and how many trips the tour has.
pub(crate) fn tour_choice(app: &App, k: usize) -> Option<(usize, usize, usize, usize)> {
    let (line, tour) = tour_at(app, k)?;
    let sch = app.schedule.as_ref()?;
    let trips = sch.tour_trip_count(&line, &tour);
    let (stop, trip) = match app.list_kind.as_ref() {
        Some(ListKind::Tours(_, Some(p))) if p.0 == tour => (p.1, p.2),
        _ => (0, sch.tour_trip_now(&line, &tour, app.clock.time)),
    };
    let trip = trip.min(trips.saturating_sub(1));
    let n = sch.tour_trip_stops(&line, &tour, trip).len();
    (n > 0).then(|| (n, stop.min(n - 1), trip, trips))
}

/// Start the tour at stop number `chosen` of trip number `trip` of the tour (the trip chosen
/// by its time): the duty goes on from that stop, the bus stays where it is.
pub(crate) fn start_duty_at(app: &mut App, line: &str, tour: &str, trip: usize, chosen: usize) {
    let now = app.clock.time;
    let at = tour_start_of(app, line, tour);
    let Some((k, j)) = app.schedule.as_ref().and_then(|s| s.tour_trip_stops(line, tour, trip).get(chosen).map(|x| (x.0, x.1))) else {
        return start_duty(app, line, tour);
    };
    let (Some(w), Some(sch)) = (app.world.clone(), app.schedule.as_mut()) else { return };
    let mut d = match sch.player_duty(&w, line, tour, at, None, false) {
        Ok(d) => d,
        Err(e) => {
            app.service_msg = Some((format!("No duty: {e}"), 8.0));
            return;
        }
    };
    // (no teleport: the stop chosen is the one the bus drives to next)
    d.start_at_here(k, j);
    if let Some(p) = app.player.as_mut() {
        d.update(&mut p.vehicle, now);
        let (trip, stop) = d.trip_for_ibis();
        p.set_duty_destination(trip, stop);
        if let Some(w) = app.world.as_ref() {
            let mut fonts = w.fonts.lock();
            if let Err(e) = crate::schedule_paper::update_vehicle(&mut p.vehicle, &d, &mut fonts) {
                log::warn!("driver timetable paper: {e:#}");
            }
        }
    }
    app.args.line = Some(line.to_string());
    app.args.tour = Some(tour.to_string());
    app.duty = Some(d);
    app.service_msg = Some((format!("Line {line}, tour {}", tour.trim()), 4.0));
}

fn start_duty(app: &mut App, line: &str, tour: &str) {
    let (Some(w), Some(sch)) = (app.world.clone(), app.schedule.as_mut()) else { return };
    let now = app.clock.time;
    match sch.player_duty(&w, line, tour, now, None, false) {
        Ok(mut d) => {
            if let Some(p) = app.player.as_mut() {
                d.update(&mut p.vehicle, now);
                let (trip, stop) = d.trip_for_ibis();
                p.set_duty_destination(trip, stop);
                let mut fonts = w.fonts.lock();
                if let Err(e) = crate::schedule_paper::update_vehicle(
                    &mut p.vehicle,
                    &d,
                    &mut fonts,
                ) {
                    log::warn!("driver timetable paper: {e:#}");
                }
            }
            app.args.line = Some(line.to_string());
            app.args.tour = Some(tour.to_string());
            app.duty = Some(d);
            app.service_msg = Some((format!("Line {line}, tour {}", tour.trim()), 4.0));
        }
        Err(e) => app.service_msg = Some((format!("No duty: {e}"), 8.0)),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn escape_fov_updates_the_active_projection_and_can_restore_geometry() {
        let mut settings = crate::settings::Settings::default();
        assert_eq!(super::set_camera_fov(&mut settings, 50.0).0, "fov");
        settings.triple.enabled = true;
        assert_eq!(
            super::set_camera_fov(&mut settings, 75.0).0,
            "triple_fov_deg"
        );
        assert_eq!(settings.fov, 50.0);
        assert_eq!(settings.triple.fov_deg, 75.0);
        super::set_camera_fov(&mut settings, 0.0);
        assert_eq!(settings.triple.fov_deg, 0.0);
    }
    #[test]
    fn steps_wrap_round() {
        assert_eq!(super::next_step(&super::SPEEDS, 1.0), 2.0);
        assert_eq!(super::next_step(&super::SPEEDS, 15.0), 1.0);
        assert_eq!(super::next_step(&super::TRAFFIC, 35), 50);
    }

    #[test]
    fn lines_sort_as_numbers() {
        let mut v = vec!["13N", "5", "137", "N30", "92"];
        v.sort_by(|a, b| super::natural(a, b));
        assert_eq!(v, vec!["5", "13N", "92", "137", "N30"]);
    }

    #[test]
    fn symbols_are_not_forced_through_numeric_ibis_lines() {
        assert!(super::numeric_ibis_line("10"));
        assert!(super::numeric_ibis_line("10E"));
        assert!(super::numeric_ibis_line("X10"));
        assert!(!super::numeric_ibis_line("-10"));
        assert!(!super::numeric_ibis_line("10-"));
        assert!(!super::numeric_ibis_line("EXP"));
    }
}
