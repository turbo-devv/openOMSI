//! Scheduled AI buses: the map's timetable lines (`TTData`) put buses on their tracks at the
//! tour departure times; they follow the track lanes and stop at the trip's stations.

use crate::scene::World;
use crate::traffic::Traffic;
use hashbrown::{HashMap, HashSet};
use omsi_render::{Renderer, Scene};
use omsi_sim::traffic::{LaneKey, Network};
use omsi_sim::VehicleType;
use omsi_timetable::TimetableData;
use std::path::Path;
use std::sync::Arc;

struct Departure {
    /// Seconds since midnight.
    time: f64,
    trip: usize,
    /// The trip's profile the tour runs it with (`[addtrip]`).
    profile: usize,
    line: String,
    ai_group: String,
    tour: String,
    /// The tour's validity mask (bits 0-6 Monday..Sunday, 7 public holiday, 8 school
    /// holidays, 9 school days): every tour's departures are kept, and which run is decided
    /// by the day (`Schedule::runs`), so a session carries on past midnight.
    mask: i32,
    spawned: bool,
}

/// One step of a trip's route: a lane in map terms (None when the tile index is not in the
/// map's list) and the leg between two stations it belongs to (0 for a track).
#[derive(Debug, Clone, Copy)]
struct Step {
    key: Option<LaneKey>,
    leg: usize,
    /// The path's length as the timetable file has it (m).
    length: f64,
}

/// A route step as the loaded network has it.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Slot {
    /// The lane it runs on.
    Lane(usize),
    /// Its tile is part of the map, but has not brought its lanes yet.
    Waiting,
    /// Not in the map (a tile or path that does not exist): passed over, as a whole-map load
    /// passes it over.
    Absent,
}

/// What became of a departure that was due.
enum Placed {
    Spawned,
    /// The part of the route the bus is on now is not loaded: try again later.
    Wait,
    /// Nothing to do any more (the trip is over, or has no route or no vehicle).
    Drop,
    /// A car stands where the bus would appear: try again at the next call.
    Busy,
}

/// A scheduled bus whose route stops short of a tile that has not brought its lanes yet: the
/// route is carried on as the tiles come.
struct RunningTrip {
    car: u64,
    steps: Vec<Step>,
    /// The first step its route does not have yet.
    next: usize,
    /// The trip's stations with their departure times, and which of them the bus stops at
    /// already or has passed.
    stations: Vec<(i64, f64)>,
    served: Vec<bool>,
}

/// When a trip's bus is at each of its stations, as OMSI's timetable has it: the profile
/// gives the trip's duration and, for some stations, the minute the bus arrives or leaves
/// (`[profile_man_arr_time]`, `[profile_man_dep_time]`, minutes after the trip's start);
/// the stations in between are timed by the lengths of the station links (Spandau's line 5
/// gives nearly every station its minute; these used to be ignored for the whole duration
/// split by the link lengths, and the tours ran their first profile whatever `[addtrip]`
/// said). A station marked `[profile_otherstopping] 2` is passed without a stop (every
/// station of a depot run).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TripTimes {
    /// Seconds after the trip's departure: (arrival, departure) per station.
    pub stations: Vec<(f64, f64)>,
    /// Whether the bus stops at the station.
    pub stops: Vec<bool>,
    /// `[profile_otherstopping]` per station (0 when not given): 1 and 4 stop whoever
    /// wants to get on or off, 2 is passed, 3 is served when the bus would be more than 20 s
    /// early (Omsi.exe 0x7da6f0 .. 0x7da8bf; see `bus_service::BusService::must_serve`).
    pub kinds: Vec<u8>,
    /// Seconds from the departure to the arrival at the last station.
    pub duration: f64,
}

impl TripTimes {
    pub fn new(
        stations: &[i64],
        profile: Option<&omsi_timetable::TripProfile>,
        link_length: &dyn Fn(i64, i64) -> Option<f64>,
    ) -> TripTimes {
        let n = stations.len();
        let duration = profile
            .map(|p| p.factor as f64 * 60.0)
            .filter(|d| *d > 0.0)
            .unwrap_or(600.0);
        let mut arr: Vec<Option<f64>> = vec![None; n];
        let mut dep: Vec<Option<f64>> = vec![None; n];
        let mut stops = vec![true; n];
        let mut kinds = vec![0u8; n];
        if let Some(p) = profile {
            let at = |i: i32| usize::try_from(i).ok().filter(|i| *i < n);
            for (i, m) in &p.man_arr_time {
                if let Some(i) = at(*i) {
                    arr[i] = Some(*m as f64 * 60.0);
                }
            }
            for (i, m) in &p.man_dep_time {
                if let Some(i) = at(*i) {
                    dep[i] = Some(*m as f64 * 60.0);
                }
            }
            for (i, v) in &p.other_stopping {
                if let Some(i) = at(*i) {
                    kinds[i] = (*v).clamp(0, 255) as u8;
                    if *v == 2 {
                        stops[i] = false;
                    }
                }
            }
        }
        // the trip leaves its first station at its departure and reaches the last one after
        // the profile's duration, unless the profile times them itself
        if n > 0 && arr[0].is_none() && dep[0].is_none() {
            dep[0] = Some(0.0);
        }
        if n > 1 && arr[n - 1].is_none() && dep[n - 1].is_none() {
            arr[n - 1] = Some(duration);
        }
        // the distance to every station along the links (a missing link counts 500 m)
        let mut along = vec![0.0f64; n];
        for i in 1..n {
            along[i] = along[i - 1]
                + link_length(stations[i - 1], stations[i])
                .unwrap_or(500.0)
                .max(1.0);
        }
        let timed: Vec<usize> = (0..n)
            .filter(|&i| arr[i].is_some() || dep[i].is_some())
            .collect();
        let mut out = Vec::with_capacity(n);
        let mut last = 0.0f64;
        for i in 0..n {
            let (a, d) = match (arr[i], dep[i]) {
                (Some(a), Some(d)) => (a, d.max(a)),
                (Some(a), None) => (a, a),
                (None, Some(d)) => (d, d),
                (None, None) => {
                    let p = timed.iter().rev().find(|&&k| k < i).copied();
                    let q = timed.iter().find(|&&k| k > i).copied();
                    let t = match (p, q) {
                        (Some(p), Some(q)) => {
                            let t0 = dep[p].or(arr[p]).unwrap_or(0.0);
                            let t1 = arr[q].or(dep[q]).unwrap_or(t0);
                            let span = along[q] - along[p];
                            if span > 0.0 {
                                t0 + (t1 - t0) * (along[i] - along[p]) / span
                            } else {
                                t0
                            }
                        }
                        (Some(p), None) => dep[p].or(arr[p]).unwrap_or(0.0),
                        (None, Some(q)) => arr[q].or(dep[q]).unwrap_or(0.0),
                        (None, None) => 0.0,
                    };
                    (t, t)
                }
            };
            // never back in time
            let a = a.max(last);
            let d = d.max(a);
            last = d;
            out.push((a, d));
        }
        // a trip with a route of stations lasts until it reaches the last one
        let duration = if n > 1 { out[n - 1].0 } else { duration };
        TripTimes {
            stations: out,
            stops,
            kinds,
            duration: duration.max(1.0),
        }
    }
}

/// The stations a trip calls at: its `[station_typ2]` objects, or the objects of the older
/// `[station]` records the trains, the ferry and the U-Bahn of Spandau still use (their first
/// line is the object id).
/// The station targets of [`Schedule::stop_targets`] from the trips' stops and termini.
/// A stop is never a target of itself or of another stop of the same name (the platforms
/// of one station, the first and last stop of a circular line): somebody waiting there who
/// drew it got in, found the bus at their stop and got straight off again, over and over,
/// every one of them adding another pedestrian (#795).
fn station_targets(trips: impl Iterator<Item = (Vec<i64>, String)>, name_of: impl Fn(i64) -> String) -> HashMap<i64, Vec<(String, HashSet<String>)>> {
    let mut named: HashMap<i64, Vec<(String, HashSet<String>)>> = HashMap::new();
    for (stations, terminus) in trips {
        for (k, from) in stations.iter().enumerate() {
            let here = name_of(*from);
            let targets = named.entry(*from).or_default();
            for to in &stations[k + 1..] {
                let to = name_of(*to);
                if to == here {
                    continue;
                }
                match targets.iter_mut().find(|t| t.0 == to) {
                    Some(t) => {
                        t.1.insert(terminus.clone());
                    }
                    None => targets.push((to, HashSet::from_iter([terminus.clone()]))),
                }
            }
        }
    }
    named
}

fn trip_stations(trip: &omsi_timetable::Trip) -> Vec<i64> {
    if !trip.stations.is_empty() {
        return trip.stations.clone();
    }
    trip.stations_legacy
        .iter()
        .filter_map(|s| s.first().and_then(|id| id.trim().parse::<i64>().ok()))
        .collect()
}

/// How far from a route a bus stop may stand when the route is only a part of the trip (a
/// stop of the missing part would otherwise be put on the nearest point of this one).
const STOP_REACH: f64 = 25.0;

/// How far ahead (s) the timetable reads and uploads the vehicles of its next departures: a
/// layover bus stands at its first stop a quarter of an hour early.
const FLEET_AHEAD: f64 = 25.0 * 60.0;
/// [`FLEET_AHEAD`], or `OMSI_FLEET_AHEAD` minutes (for tests).
fn fleet_ahead() -> f64 {
    omsi_cfg::env::var("OMSI_FLEET_AHEAD")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .map(|m| m * 60.0)
        .unwrap_or(FLEET_AHEAD)
}

/// A vehicle set nobody has drawn for this long, and that no departure of the next
/// minutes wants, leaves the GPU.
const FLEET_IDLE: std::time::Duration = std::time::Duration::from_secs(90);

/// The vehicle a departure is driven with. A tour keeps its bus all day, as in OMSI: the
/// choice comes from the tour, not from the order the departures happen to spawn in, so the
/// timetable knows which vehicles its next minutes need before they are due.
struct Choice {
    ty: Arc<VehicleType>,
    number: Option<(String, String)>,
    hof: Option<Arc<omsi_vehicle::Hof>>,
    scheme: Option<usize>,
    /// A `.zug` train: its cars, the first one being `ty`.
    train: Option<Vec<(Arc<VehicleType>, bool)>>,
}

/// The bus stands next to the kerb: the pole's offset less half a bus width and a gap; only
/// where the pole is clearly off the lane (a bay).
///
/// A pole further off than a bay's width stands behind the pavement or the verge (the
/// stop objects of many maps are placed there): the bus stays in its lane at the kerb then.
/// Taken as a bay up to 4 m wide, the bus pulled out over the kerb onto the grass.
///
/// Not at all, now: a timetable bus stays on its path at the stop, as OMSI's do (a
/// map's bus bay is a spline of its own that the route runs through). The pole's offset
/// says nothing about where the kerb is - most stand behind the pavement - and a bus
/// moved 1.6 m to the right of its lane drove along with its right wheels on the pavement.
/// Stops moved `shift` metres back along `route` (the lanes the stops' route indices less
/// `base` count in): where the vehicle's origin comes to rest (`bus_service::stop_shift`).
/// One that comes to lie before the route's first lane keeps a distance below zero on it.
fn shift_stops(net: &Network, route: &[usize], base: usize, stops: &mut [(usize, f32, f32, f64, i64, f32)], shift: f32) {
    if shift.abs() < 1e-3 {
        return;
    }
    for st in stops.iter_mut() {
        let (mut k, mut ss) = (st.0.saturating_sub(base), st.1 - shift);
        while ss < 0.0 && k > 0 && k <= route.len() - 1 {
            k -= 1;
            ss += net.lanes[route[k]].length();
        }
        while ss > 0.0 && k + 1 < route.len() && ss > net.lanes[route[k]].length() {
            ss -= net.lanes[route[k]].length();
            k += 1;
        }
        st.0 = base + k;
        st.1 = ss;
    }
}

fn bay_offset(lat: f32) -> f32 {
    lat
}

/// Where a timetable bus stands across its lane at a stop, as Omsi.exe puts it
/// (0x7dac5e..0x7dae81): its kerb-side flank 0.3 m past the `[busstop]` box's centre -
/// `lat` less half its `[boundingbox]` width plus 0.3 on the right (the other way round
/// where traffic keeps left), from the box's offset `lat` off the path (right positive);
/// a railway vehicle keeps to its track. OMSI clamps it only to the room beside other
/// vehicles, not to a kerb: the bus pulls into the bay whether or not a path leads there
/// (#241). (openOMSI kept it on its path before - a map whose box stood behind the
/// pavement had its buses on the pavement - but OMSI does the same there.)
fn bay_for(lat: f32, ty: &omsi_sim::VehicleType, rail: bool, left_hand: bool) -> f32 {
    if rail || !lat.is_finite() {
        return 0.0;
    }
    let hw = ty.def.bounding_box.map(|b| b[0] * 0.5).unwrap_or(1.25);
    if left_hand {
        lat + hw - 0.3
    } else {
        lat - hw + 0.3
    }
}

/// The stops' raw box offsets (see `bay_offset`) made the vehicle's bay offsets, and the
/// stops moved to where its origin comes to rest (`shift_stops`).
fn place_stops(net: &Network, route: &[usize], base: usize, stops: &mut [(usize, f32, f32, f64, i64, f32)], ty: &omsi_sim::VehicleType, rail: bool) {
    for st in stops.iter_mut() {
        st.2 = bay_for(st.2, ty, rail, net.left_hand);
    }
    shift_stops(net, route, base, stops, crate::bus_service::stop_shift(ty, rail));
}

/// Where on `route` the bus stop at `pos` is: (route index, distance along that lane, lateral
/// offset); None when it is further than `reach` from the route.
/// Where the stop at `pos` lies on `route`, not before route index `from` (the stops come
/// in the trip's order): on the side of the road it stands, see
/// `Network::project_stop_on_route`.
fn project_stop(
    net: &Network,
    route: &[usize],
    pos: glam::DVec3,
    reach: Option<f64>,
    from: usize,
) -> Option<(usize, f32, f32)> {
    net.project_stop_on_route(route, pos, reach, from)
}

pub struct Schedule {
    pub data: TimetableData,
    departures: Vec<Departure>,
    /// Depot vehicles per AI group: (type, its fleet from the ailists, depot file).
    depots: HashMap<
        String,
        Vec<(
            Arc<VehicleType>,
            Vec<omsi_map::DepotEntry>,
            Option<Arc<omsi_vehicle::Hof>>,
        )>,
    >,
    tile_coords: Vec<(i32, i32)>,
    next_number: usize,
    /// Trains per AI group: list of (car type, reversed), first car leads.
    trains: HashMap<String, Vec<Vec<(Arc<VehicleType>, bool)>>>,
    /// Plain `[aigroup_2]` vehicle pools, loaded the first time a trip asks for one
    /// (the Tegel approaches are flown by the group's own aircraft, not by depot buses).
    pools: HashMap<String, Vec<Arc<VehicleType>>>,
    /// Departures that are due but not on the road yet. Putting twenty minutes of a Berlin
    /// timetable on the map at once costs several seconds in one frame, so they are spawned
    /// a few at a time.
    pending: std::collections::VecDeque<usize>,
    /// Per departure: the previous departure of the same tour (its bus is the same one).
    tour_prev: Vec<Option<usize>>,
    /// Due departures whose bus would be on a part of its route that is not loaded: tried
    /// again when tiles bring lanes and as time moves the bus on.
    waiting: Vec<usize>,
    /// Buses on the road whose route is still to be carried on.
    running: Vec<RunningTrip>,
    /// `Traffic::lanes_generation` when the waiting departures and the running routes were
    /// last looked at, and the time of day of the last retry.
    seen_generation: u64,
    last_retry: f64,
    /// The departure each timetable bus on the road runs (by car id): a bus the traffic took
    /// off with its unloaded tile goes back to `waiting` and returns with the tiles, at the
    /// place its timetable puts it then.
    car_departure: HashMap<u64, usize>,
    /// Waiting departures whose vehicle is timed to reach loaded lanes at this time of day:
    /// they are tried again then, so that a plane coming in over tiles nobody loads appears
    /// where its path enters the loaded ones, not up to half a minute later in mid-air.
    retry_at: HashMap<usize, f64>,
    /// Vehicle sets being read ahead on the workers, with the type to upload them with, and
    /// those read and waiting for their upload.
    fleet_reading: HashMap<crate::scene::VehicleKey, Arc<VehicleType>>,
    fleet_ready: Arc<parking_lot::Mutex<Vec<crate::scene::VehicleKey>>>,
    /// Time of day of the last look at the next departures' vehicles.
    fleet_check: f64,
    /// Per trip and profile: when its bus is at its stations.
    times: Vec<Vec<TripTimes>>,
    /// Per bus stop (map object id): the trips that call there, as (trip, station index).
    visits: HashMap<i64, Vec<(usize, usize)>>,
    /// Per trip: today's departures that run it.
    trip_departures: Vec<Vec<usize>>,
    /// When the departure boards were last made (time of day).
    boards_made: f64,
    /// The tour the player drives (line, tour): the timetable does not run it as well.
    player_tour: Option<(String, String)>,
    /// With a single trip picked: the departure (s of the day) of that trip; the rest of
    /// the tour stays the AI's.
    player_departure: Option<f64>,
    /// A player tour was just taken over: its buses already on the road go at the next tick.
    purge_player_tour: bool,
    /// LAN play (host): the tours the other players drive (line, tour; lower case), left to
    /// them like our own.
    lan_tours: HashSet<(String, String)>,
    /// Time of today's timetable at the last tick (s).
    last_tod: f64,
    /// The lines the date's chrono folders take off the timetable, with the folder that does
    /// it: why a duty on such a line cannot be driven.
    deactivated: Vec<(String, std::path::PathBuf)>,
    /// Stations some trip stops at on its way or ends at (not only starts from).
    served: std::collections::HashSet<i64>,
    /// Per first station: whether another trip's bus stops there or within a bus length
    /// or two of it (maps often put one stop object per line at the same kerb), once its
    /// position is known.
    shared_stand: HashMap<i64, bool>,
    /// Departures queued while the map loads: their buses may appear in view.
    startup: std::collections::HashSet<usize>,
    /// Layover departures whose stand was taken: they come at their departure time.
    later_layover: std::collections::HashSet<usize>,
    /// Per departure: the next departure of the same tour (its bus takes it on).
    tour_next: Vec<Option<usize>>,
    /// Departures due while their tour's bus is still on its previous trip: that bus takes
    /// them on when it gets there, as in OMSI a tour keeps its bus from trip to trip.
    awaiting: std::collections::HashSet<usize>,
    /// The map's holidays, for the day's tours.
    calendar: omsi_map::Calendar,
    /// The date (yyyymmdd) the departures are for, and the mask bits it selects (day,
    /// school); `set_day` moves them on at midnight.
    day: i32,
    day_bits: (i32, i32),
    /// The weekday bit of the next day (night tours run on into it).
    next_day_bit: i32,
    /// Where today's midnight lies on the traffic's clock (`Traffic::day_time` counts on past
    /// 24:00): a departure leaves at `day_base + time` (`dep_time`), and the date moves on
    /// when the clock passes the next midnight.
    day_base: f64,
    /// The current date (its time of day is not used).
    date_clock: omsi_sim::SimClock,
    /// The map's `car_use/*.ocu`: which vehicles serve which line's tours.
    car_use: Vec<omsi_timetable::CarUse>,
    /// Per tour (`tour_key_of`): the depot vehicle (index in its group) and fleet number
    /// it runs with today - from `car_use`, else drawn the first time the tour is due. A
    /// number is given to one tour only (`used_numbers`), as in OMSI:
    /// hashing each tour to a number put the same fleet number on two buses at once.
    tour_vehicle: HashMap<u64, (usize, usize)>,
    used_numbers: HashSet<(String, String)>,
}

/// A tour's bus waits at the end of a trip for the next one of its tour when that leaves
/// within this many seconds; for a longer break it goes (and a bus comes back for it).
const TOUR_LAYOVER_MAX: f64 = 30.0 * 60.0;

/// A tour's bus takes its next trip on only from one of the trip's first lanes (or a short
/// way onto one of them): Omsi.exe starts every trip at its beginning.
const TOUR_ENTRY_LANES: usize = 4;

/// Where on the next trip's route `section` the tour's bus standing on `lane` takes it on:
/// only on one of its first lanes. A tour repeating the same trip leaves its bus on the
/// route's last lane, and taken on there the trip was over at once - the bus stood at the
/// last stop for good, and every bus behind it queued (#976).
fn tour_entry(section: &[usize], lane: usize) -> Option<usize> {
    section.iter().take(TOUR_ENTRY_LANES).position(|&l| l == lane)
}

/// How early a bus waits at its first stop for its departure (s): a quarter of an hour at a
/// stand of its own, a minute where other buses stop as well (a layover bus there made
/// every bus of the other lines queue behind it until it left).
const LAYOVER: f64 = 900.0;
const LAYOVER_SHARED: f64 = 60.0;

impl Schedule {
    /// `clock` gives the date: tours carry a validity mask (bits 0-6 Monday…Sunday, 7 public
    /// holiday, 8 school holidays, 9 school days) that selects which run today.
    pub fn new(root: &Path, world: &World, clock: &omsi_sim::SimClock) -> Schedule {
        let chrono_dirs = world.chrono_dirs.read().clone();
        let deactivated = omsi_map::chrono_deactivated_lines(&chrono_dirs);
        let data = TimetableData::load_with_chrono(&world.map_dir, &chrono_dirs, &deactivated);
        for e in &data.errors {
            log::warn!("timetable: {e}");
        }
        let calendar =
            omsi_map::Calendar::load(&world.map_dir.join("Holidays.txt")).unwrap_or_default();
        let car_use = omsi_timetable::CarUse::load_dir(&world.map_dir);
        // station link lengths, the first link of a pair counts (as the routes take it)
        let mut links: HashMap<(i64, i64), f64> = HashMap::new();
        for l in &data.stn_links {
            links.entry((l.from_id, l.to_id)).or_insert(l.length);
        }
        let link_length = |a: i64, b: i64| links.get(&(a, b)).copied();
        let times: Vec<Vec<TripTimes>> = data
            .trips
            .iter()
            .map(|t| {
                let stations = trip_stations(t);
                if t.profiles.is_empty() {
                    vec![TripTimes::new(&stations, None, &link_length)]
                } else {
                    t.profiles
                        .iter()
                        .map(|p| TripTimes::new(&stations, Some(p), &link_length))
                        .collect()
                }
            })
            .collect();
        let mut visits: HashMap<i64, Vec<(usize, usize)>> = HashMap::new();
        for (ti, t) in data.trips.iter().enumerate() {
            for (k, sid) in trip_stations(t).iter().enumerate() {
                visits.entry(*sid).or_default().push((ti, k));
            }
        }
        let date = clock.date_code();
        let weekday = clock.weekday();
        let holiday = calendar.is_holiday(date);
        let school_holiday = calendar.in_holiday_range(date);
        let (day_bit, school_bit) = day_bits(&calendar, clock);
        let mut departures = Vec::new();
        let mut skipped_tours = 0;
        for line in &data.lines {
            for tour in &line.tours {
                let mask = tour.extra.trim().parse::<i32>().unwrap_or(1023);
                if mask & day_bit == 0 || mask & school_bit == 0 {
                    skipped_tours += 1;
                }
                for t in &tour.trips {
                    if let Some(ti) = data
                        .trips
                        .iter()
                        .position(|x| x.name.eq_ignore_ascii_case(&t.trip))
                    {
                        let profile = usize::try_from(t.profile)
                            .unwrap_or(0)
                            .min(times[ti].len() - 1);
                        departures.push(Departure {
                            time: t.departure as f64 * 60.0,
                            trip: ti,
                            profile,
                            line: line.name.clone(),
                            ai_group: tour.ai_group.clone(),
                            tour: tour.number.clone(),
                            mask,
                            spawned: false,
                        });
                    }
                }
            }
        }
        departures.sort_by(|a, b| a.time.total_cmp(&b.time));
        let mut trip_departures = vec![Vec::new(); data.trips.len()];
        for (i, d) in departures.iter().enumerate() {
            trip_departures[d.trip].push(i);
        }
        // the previous departure of each tour, looked up once: the layover check used to
        // walk all 3600 Spandau departures for each of them, every two seconds - 1.5 fps
        let mut tour_prev = vec![None; departures.len()];
        let mut last_of: HashMap<(String, String), usize> = HashMap::new();
        for (i, d) in departures.iter().enumerate() {
            let key = (d.line.clone(), d.tour.clone());
            tour_prev[i] = last_of.get(&key).copied();
            last_of.insert(key, i);
        }
        let mut tour_next = vec![None; departures.len()];
        for (i, p) in tour_prev.iter().enumerate() {
            if let Some(p) = p {
                tour_next[*p] = Some(i);
            }
        }
        let mut depots = HashMap::new();
        let mut warned: HashSet<String> = HashSet::new();
        let mut hof_cache: HashMap<(std::path::PathBuf, String), Option<Arc<omsi_vehicle::Hof>>> =
            HashMap::new();
        // a bus file several depots (or typgroups) list is one type: loaded once
        let mut loaded: HashMap<std::path::PathBuf, Option<Arc<VehicleType>>> = HashMap::new();
        let mut load = |path: &std::path::Path| -> Result<Arc<VehicleType>, String> {
            loaded
                .entry(path.to_path_buf())
                .or_insert_with(|| {
                    VehicleType::load_ai(root, path)
                        .map(Arc::new)
                        .map_err(|e| log::debug!("{}: {e}", path.display()))
                        .ok()
                })
                .clone()
                .ok_or_else(|| "could not be loaded".to_string())
        };
        {
            let lists = &world.ailists;
            for g in lists.groups.iter().filter(|g| g.is_depot) {
                let mut vehicles = Vec::new();
                for tg in &g.typgroups {
                    let path = omsi_cfg::resolve_path(root, &tg.file);
                    match load(&path) {
                        Ok(t) => {
                            let hof = g
                                .hof
                                .as_ref()
                                .and_then(|h| depot_file(&mut hof_cache, t.def.dir(), h));
                            vehicles.push((t, tg.entries.clone(), hof));
                        }
                        Err(e) => {
                            if !warned.contains(&tg.file) {
                                log::warn!("depot vehicle {}: {e}", tg.file);
                                warned.insert(tg.file.clone());
                            }
                        }
                    }
                }
                depots.insert(g.name.to_ascii_lowercase(), vehicles);
            }
        }
        // train groups: [aigroup_2] entries pointing at .zug files
        let mut trains: HashMap<String, Vec<Vec<(Arc<VehicleType>, bool)>>> = HashMap::new();
        {
            let lists = &world.ailists;
            for g in lists.groups.iter().filter(|g| !g.is_depot) {
                for v in &g.vehicles {
                    if !v.file.to_ascii_lowercase().ends_with(".zug") {
                        continue;
                    }
                    let path = omsi_cfg::resolve_path(root, &v.file);
                    let Ok(train) = omsi_vehicle::vehicle::Train::load(&path) else {
                        continue;
                    };
                    let mut cars = Vec::new();
                    let mut pick = 0usize;
                    for (file, rev) in &train.cars {
                        // an entry is a vehicle file, or the name of a vehicle pool group
                        let p = omsi_cfg::resolve_path(root, file);
                        let path = if omsi_cfg::vfs::is_file(&p) {
                            Some(p)
                        } else {
                            lists
                                .groups
                                .iter()
                                .find(|g| {
                                    g.name.eq_ignore_ascii_case(file.trim())
                                        && !g.vehicles.is_empty()
                                })
                                .map(|g| {
                                    pick += 1;
                                    omsi_cfg::resolve_path(
                                        root,
                                        &g.vehicles[pick % g.vehicles.len()].file,
                                    )
                                })
                        };
                        let Some(path) = path else {
                            log::warn!("train car {file}: no such file or group");
                            continue;
                        };
                        match load(&path) {
                            Ok(t) => cars.push((t, *rev)),
                            Err(e) => log::warn!("train car {}: {e}", file),
                        }
                    }
                    if !cars.is_empty() {
                        trains
                            .entry(g.name.to_ascii_lowercase())
                            .or_default()
                            .push(cars);
                    }
                }
            }
        }
        let tile_coords = world.global.raw_tiles.clone();
        if omsi_cfg::env::var_os("OMSI_PROFILE").is_some() {
            let mut seen: HashSet<*const VehicleType> = HashSet::new();
            let mut bytes = 0usize;
            for t in depots
                .values()
                .flatten()
                .map(|v| &v.0)
                .chain(trains.values().flatten().flatten().map(|c| &c.0))
            {
                if seen.insert(Arc::as_ptr(t)) {
                    bytes += t.mesh_bytes();
                }
            }
            log::info!(
                "timetable: {} vehicle types loaded, {:.0} MB of meshes on the CPU",
                seen.len(),
                bytes as f64 / 1e6
            );
        }
        let today = departures.iter().filter(|d| d.mask & day_bit != 0 && d.mask & school_bit != 0).count();
        log::info!("timetable: {} lines, {} trips, {} tracks, {} departures today (weekday {weekday}, holiday {holiday}, school holidays {school_holiday}; {skipped_tours} tours not today), {} depot groups", data.lines.len(), data.trips.len(), data.tracks.len(), today, depots.len());
        let served = data
            .trips
            .iter()
            .flat_map(|t| trip_stations(t).into_iter().skip(1))
            .collect();
        let mut s = Schedule {
            data,
            departures,
            depots,
            tile_coords,
            next_number: 0,
            trains,
            pools: HashMap::new(),
            pending: Default::default(),
            tour_prev,
            waiting: Vec::new(),
            running: Vec::new(),
            seen_generation: 0,
            last_retry: f64::NEG_INFINITY,
            car_departure: HashMap::new(),
            retry_at: HashMap::new(),
            fleet_reading: HashMap::new(),
            fleet_ready: Default::default(),
            fleet_check: f64::NEG_INFINITY,
            times,
            visits,
            trip_departures,
            boards_made: f64::NEG_INFINITY,
            player_tour: None,
            player_departure: None,
            purge_player_tour: false,
            lan_tours: HashSet::new(),
            last_tod: 0.0,
            deactivated,
            served,
            shared_stand: HashMap::new(),
            startup: Default::default(),
            later_layover: Default::default(),
            tour_next,
            awaiting: Default::default(),
            calendar,
            day: date,
            day_bits: (day_bit, school_bit),
            next_day_bit: 1 << ((clock.weekday() + 1) % 7),
            day_base: 0.0,
            date_clock: clock.clone(),
            car_use,
            tour_vehicle: HashMap::new(),
            used_numbers: HashSet::new(),
        };
        s.assign_car_use();
        s
    }

    /// The day's vehicles of the tours the map's `car_use` names (the original, run
    /// at load and when the date changes): for every record in force whose line runs,
    /// first its `[number_tour]` pairs (a fleet number for a tour), then for each other
    /// tour of the line, with the probability `[types_prefered]` gives (1 for
    /// `[onlytypes]`), a fleet number of one of the listed types from the tour's depot
    /// group that no tour has yet. Other tours draw theirs when they are first due.
    fn assign_car_use(&mut self) {
        self.tour_vehicle.clear();
        self.used_numbers.clear();
        let date = self.day;
        let mut n_fixed = 0;
        let mut n_typed = 0;
        for cu in self.car_use.iter().filter(|c| c.valid_on(date)) {
            let Some(line) = self.data.lines.iter().find(|l| l.name.trim().eq_ignore_ascii_case(cu.line.trim())) else { continue };
            // [number_tour]: this tour runs with this vehicle
            for (number, tour) in &cu.number_tour {
                let Some(t) = line.tours.iter().find(|t| t.number.trim().eq_ignore_ascii_case(tour.trim())) else { continue };
                let group = t.ai_group.to_ascii_lowercase();
                let key = tour_key_of(&group, &line.name, &t.number);
                if self.tour_vehicle.contains_key(&key) || self.used_numbers.contains(&(group.clone(), number.trim().to_string())) {
                    continue;
                }
                let found = self.depots.get(&group).and_then(|v| {
                    v.iter().enumerate().find_map(|(k, (_, nums, _))| nums.iter().position(|e| e.number.trim() == number.trim()).map(|j| (k, j)))
                });
                if let Some(kj) = found {
                    self.tour_vehicle.insert(key, kj);
                    self.used_numbers.insert((group, number.trim().to_string()));
                    n_fixed += 1;
                }
            }
            // [onlytypes] / [types_prefered]: the other tours from those types
            let Some((factor, types)) = cu.types() else { continue };
            let types: Vec<String> = types.iter().map(|t| norm_vehicle_path(t)).collect();
            for t in &line.tours {
                let group = t.ai_group.to_ascii_lowercase();
                let key = tour_key_of(&group, &line.name, &t.number);
                if self.tour_vehicle.contains_key(&key) {
                    continue;
                }
                let h = mix(key ^ date as u64);
                if (h >> 11) as f64 / (1u64 << 53) as f64 > factor as f64 {
                    continue;
                }
                let Some(vehicles) = self.depots.get(&group) else { continue };
                let candidates: Vec<(usize, usize)> = vehicles
                    .iter()
                    .enumerate()
                    .filter(|(_, (ty, _, _))| {
                        let p = norm_vehicle_path(&ty.def.path.to_string_lossy());
                        types.iter().any(|w| p.ends_with(w.as_str()))
                    })
                    .flat_map(|(k, (_, nums, _))| (0..nums.len()).map(move |j| (k, j)))
                    .filter(|(k, j)| !self.used_numbers.contains(&(group.clone(), vehicles[*k].1[*j].number.trim().to_string())))
                    .collect();
                if candidates.is_empty() {
                    continue;
                }
                let (k, j) = candidates[(mix(h) % candidates.len() as u64) as usize];
                if omsi_cfg::env::var_os("OMSI_DEBUG_TRAFFIC").is_some() {
                    log::info!("car_use: line {} tour {} -> {} #{}", line.name, t.number, vehicles[k].0.def.path.display(), vehicles[k].1[j].number);
                }
                self.used_numbers.insert((group.clone(), vehicles[k].1[j].number.trim().to_string()));
                self.tour_vehicle.insert(key, (k, j));
                n_typed += 1;
            }
        }
        if n_fixed + n_typed > 0 {
            log::info!("timetable: car_use gives {n_fixed} tours their fleet number and {n_typed} a vehicle of their line's types");
        }
    }

    /// When departure `i` leaves on the traffic's clock.
    fn dep_time(&self, i: usize) -> f64 {
        self.day_base + self.departures[i].time
    }

    /// Past midnight on the traffic's clock: the next day's timetable.
    fn roll_day(&mut self, day_time: f64) {
        while day_time - self.day_base >= DAY {
            self.day_base += DAY;
            let mut c = self.date_clock.clone();
            c.paused = false;
            c.advance(DAY as f32);
            self.date_clock = c;
            let c = self.date_clock.clone();
            self.set_day(&c);
        }
    }

    /// Whether a tour is offered on the current day (the lists of lines and tours): its day
    /// mask has the day (and school day or holiday), or - for a night tour with trips after
    /// 24:00 - the next day's weekday, as the night belongs to both.
    pub(crate) fn tour_available(&self, tour: &omsi_timetable::Tour) -> bool {
        let m = tour.extra.trim().parse::<i32>().unwrap_or(1023);
        if m & self.day_bits.0 != 0 && m & self.day_bits.1 != 0 {
            return true;
        }
        let night = tour.trips.iter().any(|t| t.departure >= 24.0 * 60.0);
        night && m & self.next_day_bit != 0 && m & self.day_bits.1 != 0
    }

    /// Whether departure `i`'s tour runs on the current day.
    fn runs(&self, i: usize) -> bool {
        let m = self.departures[i].mask;
        m & self.day_bits.0 != 0 && m & self.day_bits.1 != 0
    }

    /// Move the timetable on to the clock's date when it has changed (midnight): the day's
    /// tours are chosen anew and every departure may run again. Before, the departures were
    /// made once for the start day and stayed spawned, so after the first midnight only the
    /// early-morning trips before the start time came, and on the old day's tours.
    /// Departures still queued or under way keep their state; the player's tour stays taken.
    fn set_day(&mut self, clock: &omsi_sim::SimClock) {
        let date = clock.date_code();
        if date == self.day {
            return;
        }
        self.day = date;
        self.day_bits = day_bits(&self.calendar, clock);
        self.next_day_bit = 1 << ((clock.weekday() + 1) % 7);
        let busy: HashSet<usize> = self
            .pending
            .iter()
            .chain(self.waiting.iter())
            .chain(self.awaiting.iter())
            .chain(self.later_layover.iter())
            .chain(self.car_departure.values())
            .copied()
            .collect();
        let mut n = 0;
        for i in 0..self.departures.len() {
            if busy.contains(&i) {
                continue;
            }
            let mine = self.is_player_tour(i);
            let d = &mut self.departures[i];
            if d.spawned && !mine {
                d.spawned = false;
                n += 1;
            }
        }
        self.startup.clear();
        self.boards_made = f64::NEG_INFINITY;
        self.assign_car_use();
        let today = (0..self.departures.len()).filter(|&i| self.runs(i)).count();
        log::info!("timetable: a new day ({date}): {today} departures today, {n} made ready to run again");
    }

    /// The timetable bus on the road that runs departure `k` (not one that has been let go).
    fn tour_bus(&self, k: usize, traffic: &Traffic) -> Option<usize> {
        traffic
            .cars
            .iter()
            .position(|c| c.is_bus() && !c.gone && self.car_departure.get(&c.id) == Some(&k))
    }

    /// The timetable buses at the end of their trip: each takes its tour's next trip on
    /// where it stands, or goes.
    fn tour_handover(
        &mut self,
        world: &World,
        traffic: &mut Traffic,
        renderer: &Renderer,
        scene: &mut Scene,
        day_time: f64,
    ) {
        let done: Vec<u64> = traffic.cars.iter().filter(|c| c.trip_done()).map(|c| c.id).collect();
        for id in done {
            let Some(ci) = traffic.cars.iter().position(|c| c.id == id) else { continue };
            let next = self.car_departure.get(&id).and_then(|&k| self.tour_next[k]);
            let mut taken = false;
            if let Some(j) = next {
                let d = &self.departures[j];
                let open = self.awaiting.contains(&j) || (!d.spawned && self.runs(j));
                if open && !self.is_player_tour(j) && self.day_base + d.time - day_time < TOUR_LAYOVER_MAX {
                    if let Placed::Spawned =
                        self.spawn_departure(j, world, traffic, renderer, scene, day_time, Some(ci))
                    {
                        taken = true;
                        self.departures[j].spawned = true;
                        self.awaiting.remove(&j);
                        self.pending.retain(|x| *x != j);
                        self.waiting.retain(|x| *x != j);
                        self.retry_at.remove(&j);
                        self.later_layover.remove(&j);
                    }
                }
                // its bus is not coming: the trip gets a bus of its own
                if !taken && self.awaiting.remove(&j) {
                    self.pending.push_back(j);
                }
            }
            if !taken && next.is_none() {
                // the tour's last trip is over: Omsi takes the bus (and what is coupled to
                // it) off the road at once rather than letting it drive on
                traffic.remove_car(world, renderer, scene, id);
                self.car_departure.remove(&id);
                if omsi_cfg::env::var_os("OMSI_DEBUG_TRAFFIC").is_some() {
                    log::info!("scheduled bus {id}: the last trip of its tour is over: removed");
                }
            } else if !taken {
                traffic.release(ci);
                if omsi_cfg::env::var_os("OMSI_DEBUG_TRAFFIC").is_some() {
                    log::info!("scheduled bus {id}: trip over, no next trip of its tour to take on here: it drives off");
                }
            }
        }
        // a trip whose tour's bus has gone off the road meanwhile
        if !self.awaiting.is_empty() {
            let orphans: Vec<usize> = self
                .awaiting
                .iter()
                .copied()
                .filter(|&j| self.tour_prev[j].and_then(|k| self.tour_bus(k, traffic)).is_none())
                .collect();
            for j in orphans {
                self.awaiting.remove(&j);
                self.pending.push_back(j);
            }
        }
    }

    /// The steps of a trip's route: from the trip's own track when it has one (trains,
    /// ferries, planes), else from the station links between its stops. The flag says it is
    /// a track.
    ///
    /// The track is the one the trip's `[trip]` block names on its first line (Novi Sad's
    /// trip "1 Klisa-Liman I" runs track "1_Klisa-Liman1"; the stock trains name tracks of
    /// their own name), else the one named like the trip.
    fn steps_of(&self, track_name: &str, stations: &[i64]) -> (Vec<Step>, bool) {
        let track_name = self
            .data
            .trip(track_name)
            .map(|t| t.display_name.trim())
            .filter(|n| !n.is_empty())
            .unwrap_or(track_name);
        let key = |id: f64, path: f64, tile_index: f64| {
            self.tile_coords
                .get(tile_index as usize)
                .map(|&tile| LaneKey {
                    tile,
                    id: id as i64,
                    path: path as u16,
                })
        };
        let mut steps: Vec<Step> = Vec::new();
        if let Some(track) = self.data.tracks.iter().find(|t| {
            t.path
                .file_stem()
                .map(|s| s.to_string_lossy().eq_ignore_ascii_case(track_name))
                .unwrap_or(false)
        }) {
            steps.extend(
                track
                    .entries
                    .iter()
                    .filter(|e| e.values.len() >= 5)
                    .map(|e| Step {
                        key: key(e.values[0], e.values[1], e.values[2]),
                        leg: 0,
                        length: e.values[4],
                    }),
            );
            return (steps, true);
        }
        for (leg, w) in stations.windows(2).enumerate() {
            match self
                .data
                .stn_links
                .iter()
                .find(|l| l.from_id == w[0] && l.to_id == w[1])
            {
                Some(link) => {
                    for e in &link.entries {
                        let k = key(e.values[0], e.values[1], e.values[2]);
                        // consecutive links repeat the shared lane
                        if steps.last().map(|s| s.key == k).unwrap_or(false) {
                            continue;
                        }
                        steps.push(Step {
                            key: k,
                            leg,
                            length: e.values[3],
                        });
                    }
                }
                None => log::debug!("trip {track_name}: no station link {} -> {}", w[0], w[1]),
            }
        }
        (steps, false)
    }

    /// One-way paths that a route drives the other way - their end lies where the path
    /// before it ends, their start where the next one begins - get a lane that way
    /// (`Traffic::add_reverse_twins`). OMSI's timetable buses follow their station links
    /// and tracks whichever way a path runs: Spandau's line to Kladow and a dozen Novi Sad
    /// tracks run over invisible one-way helper streets backwards, and the bus drove them
    /// forwards, against its route, and jumped back at their end.
    fn add_twins(traffic: &mut Traffic, steps: &[Step]) {
        let net = &traffic.net;
        let cands: Vec<Option<&Vec<usize>>> = steps
            .iter()
            .map(|st| st.key.and_then(|k| net.by_key.get(&k)).filter(|c| !c.is_empty()))
            .collect();
        let ends = |c: &Vec<usize>| -> Vec<glam::DVec3> {
            c.iter().flat_map(|&l| [net.lanes[l].start(), net.lanes[l].end()]).collect()
        };
        let near = |p: glam::DVec3, pts: &[glam::DVec3]| {
            pts.iter().map(|q| (*q - p).truncate().length()).fold(f64::MAX, f64::min)
        };
        let mut want = Vec::new();
        for (i, c) in cands.iter().enumerate() {
            // a path that runs both ways has its lanes already
            let Some(c) = c.filter(|c| c.len() == 1) else { continue };
            let l = &net.lanes[c[0]];
            let prev = i.checked_sub(1).and_then(|k| cands[k]).map(ends);
            let next = cands.get(i + 1).copied().flatten().map(ends);
            if prev.is_none() && next.is_none() {
                continue;
            }
            let score = |a: glam::DVec3, b: glam::DVec3| {
                prev.as_ref().map(|p| near(a, p)).unwrap_or(0.0)
                    + next.as_ref().map(|n| near(b, n)).unwrap_or(0.0)
            };
            let (fwd, bwd) = (score(l.start(), l.end()), score(l.end(), l.start()));
            if bwd + 3.0 < fwd && bwd < 6.0 {
                want.push(c[0]);
            }
        }
        if !want.is_empty() {
            traffic.add_reverse_twins(&want);
        }
    }

    /// Where a route's lanes do not join and the network has no way between them either,
    /// a connector lane across the gap (`Traffic::add_connector`), so that `bridge_gaps`
    /// finds a way to drive.
    fn add_connectors(traffic: &mut Traffic, lanes: &[usize]) {
        let net = &traffic.net;
        let holes: Vec<(usize, usize)> = lanes
            .windows(2)
            .filter(|w| !joins(net, w[0], w[1]))
            .filter(|w| {
                let gap = (net.lanes[w[1]].start() - net.lanes[w[0]].end()).truncate().length();
                way_between(net, w[0], w[1], (gap * 2.5 + 60.0) as f32).is_none()
            })
            .map(|w| (w[0], w[1]))
            .collect();
        for (a, b) in holes {
            traffic.add_connector(a, b);
        }
    }

    /// The steps as the loaded network has them, each lane's direction chosen so that it
    /// follows the lane before it (`prev` for the first) and leads into the one after it.
    /// Taking whichever direction came first - as the first step used to, with nothing
    /// before it - sent the route (and the navigator) the wrong way along a two-way street.
    fn slots(
        &self,
        world: &World,
        traffic: &Traffic,
        steps: &[Step],
        prev: Option<usize>,
    ) -> Vec<Slot> {
        let net = &traffic.net;
        // every step's candidate lanes (both directions of a two-way path)
        let cands: Vec<Result<&Vec<usize>, Slot>> = steps
            .iter()
            .map(|st| {
                let Some(key) = st.key else {
                    return Err(Slot::Absent);
                };
                match net.by_key.get(&key) {
                    Some(c) if !c.is_empty() => Ok(c),
                    _ if !traffic.lane_tiles.contains(&key.tile) && world.has_tile(key.tile) => {
                        Err(Slot::Waiting)
                    }
                    _ => Err(Slot::Absent),
                }
            })
            .collect();
        let mut out = Vec::with_capacity(steps.len());
        let mut last = prev;
        for (i, c) in cands.iter().enumerate() {
            let c = match c {
                Ok(c) => *c,
                Err(slot) => {
                    if *slot == Slot::Waiting {
                        last = None;
                    }
                    out.push(*slot);
                    continue;
                }
            };
            // the next lanes the route has (not across a gap)
            let next = cands[i + 1..].iter().find_map(|x| match x {
                Ok(n) => Some(Some(*n)),
                Err(Slot::Waiting) => Some(None),
                Err(_) => None,
            });
            let next = next.flatten();
            let score = |l: usize| -> f64 {
                let mut s = 0.0;
                if let Some(prev) = last {
                    s += (net.lanes[l].start() - net.lanes[prev].end()).length();
                }
                if let Some(next) = next {
                    let end = net.lanes[l].end();
                    s += next
                        .iter()
                        .map(|&n| (net.lanes[n].start() - end).length())
                        .fold(f64::MAX, f64::min);
                }
                s
            };
            let best = c
                .iter()
                .copied()
                .min_by(|a, b| score(*a).total_cmp(&score(*b)))
                .unwrap();
            out.push(Slot::Lane(best));
            last = Some(best);
        }
        skip_detours(net, &mut out);
        if omsi_cfg::env::var_os("OMSI_DEBUG_ROUTES").is_some() {
            // where consecutive lanes of the route do not join (a gap, or a change within the
            // same spline, which is a lane change)
            let lanes: Vec<usize> = out
                .iter()
                .filter_map(|s| {
                    if let Slot::Lane(l) = s {
                        Some(*l)
                    } else {
                        None
                    }
                })
                .collect();
            for (k, w) in lanes.windows(2).enumerate() {
                let (a, b) = (&net.lanes[w[0]], &net.lanes[w[1]]);
                let gap = (b.start() - a.end()).truncate().length();
                if gap > 2.0
                    || a.key.map(|k| (k.tile, k.id, k.path))
                    == b.key.map(|k| (k.tile, k.id, k.path))
                {
                    log::info!("route: step {k}: lane {} {:?} rev {} -> lane {} {:?} rev {}: gap {gap:.1} m, linked {}, lane change {}", w[0], a.key, a.reversed, w[1], b.key, b.reversed, a.next.contains(&w[1]), net.parallel(w[0], w[1]));
                }
            }
        }
        let waiting = out.iter().filter(|s| **s == Slot::Waiting).count();
        let absent = out.iter().filter(|s| **s == Slot::Absent).count();
        if absent > 0 || omsi_cfg::env::var_os("OMSI_DEBUG_TRAFFIC").is_some() {
            log::debug!("route: {} of {} steps on loaded lanes, {waiting} on tiles still to come, {absent} not in the map", out.len() - waiting - absent, out.len());
        }
        out
    }

    /// Upload the vehicles of the buses on the road at `day_time` and of the departures of the
    /// next minutes before the first frame, so that they spawn without a hitch; the rest of
    /// the fleet follows as its departures come near (see [`Schedule::tick`]). Uploading the
    /// whole fleet in every paint scheme up front took 109 sets and 640 MB on Ahlheim.
    ///
    /// Every type of the fleet is started once, which reads the files its scripts and
    /// displays need: done on the first bus of a type that comes along, those were frames of
    /// 60 to 170 ms in the middle of a drive.
    pub fn precache(
        &mut self,
        world: &World,
        renderer: &Renderer,
        scene: &mut Scene,
        traffic: Option<&mut crate::traffic::Traffic>,
        day_time: f64,
    ) {
        let t0 = std::time::Instant::now();
        let Some(t) = traffic else { return };
        let sets = self.upcoming_sets(world, t, day_time);
        // a few sets at a time: read on the workers, uploaded, and the copies let go
        let mut most_held = 0usize;
        for chunk in sets.chunks(3) {
            most_held = most_held.max(world.prefetch_vehicle_sets(renderer, chunk));
            for (ty, scheme) in chunk {
                world.precache_vehicle(renderer, scene, ty, *scheme);
            }
        }
        let t1 = std::time::Instant::now();
        let mut seen = std::collections::HashSet::new();
        for (ty, _, hof) in self.depots.values().flatten() {
            if !seen.insert(ty.def.path.clone()) {
                continue;
            }
            crate::traffic::warm_up(world, ty, hof.clone());
            t.prime_pull_out_room(ty, true);
            for (tr, _) in t.trailer_chain(ty) {
                if seen.insert(tr.def.path.clone()) {
                    crate::traffic::warm_up(world, &tr, None);
                }
            }
        }
        let pooled: Vec<Arc<VehicleType>> = self.pools.values().flatten().cloned().collect();
        for ty in &pooled {
            if seen.insert(ty.def.path.clone()) {
                crate::traffic::warm_up(world, ty, None);
            }
        }
        // what was read ahead and not used (textures of variants the AI never shows)
        world.forget_prefetched();
        crate::release_free_memory();
        self.fleet_check = day_time;
        log::info!("timetable fleet: {} vehicle/paint sets of the first {:.0} minutes read and uploaded in {:.1} s (at most {:.0} MB read ahead at once), a first start of every type in {:.1} s", sets.len(), fleet_ahead() / 60.0, (t1 - t0).as_secs_f32(), most_held as f64 / 1e6, t1.elapsed().as_secs_f32());
    }

    /// When departure `i`'s bus is at its trip's stations.
    /// The stations departure `i` serves whoever wants them or not (`[profile_otherstopping]`
    /// 1 or 4), and those it serves when it would be early (3), by object id.
    fn special_stops(&self, i: usize) -> (Vec<i64>, Vec<i64>) {
        let stations = trip_stations(&self.data.trips[self.departures[i].trip]);
        let kinds = &self.times_of(i).kinds;
        let of = |want: &[u8]| -> Vec<i64> {
            stations.iter().zip(kinds).filter(|(_, k)| want.contains(k)).map(|(id, _)| *id).collect()
        };
        (of(&[1, 4]), of(&[3]))
    }

    fn times_of(&self, i: usize) -> &TripTimes {
        let d = &self.departures[i];
        &self.times[d.trip][d.profile]
    }

    /// A 64-bit hash of a departure's tour (its group, line and tour number).
    fn tour_key(&self, i: usize) -> u64 {
        let d = &self.departures[i];
        tour_key_of(&d.ai_group.to_ascii_lowercase(), &d.line, &d.tour)
    }

    /// The vehicle departure `i` is driven with (see [`Choice`]).
    fn choose(&mut self, i: usize, world: &World) -> Option<Choice> {
        let group = self.departures[i].ai_group.to_ascii_lowercase();
        let h = self.tour_key(i);
        // trains: the group lists .zug files instead of depot vehicles
        let train = self
            .trains
            .get(&group)
            .and_then(|t| t.get((h % t.len().max(1) as u64) as usize).cloned());
        let (ty, numbers, hof): (
            Arc<VehicleType>,
            Vec<omsi_map::DepotEntry>,
            Option<Arc<omsi_vehicle::Hof>>,
        ) = match (&train, self.depots.get(&group).filter(|v| !v.is_empty())) {
            (Some(cars), _) => (cars[0].0.clone(), Vec::new(), None),
            (None, Some(vehicles)) => {
                // The depot's types come out in proportion to their fleets: a typgroup
                // listing 40 fleet numbers appears eight times as often as one with
                // 5, as in OMSI - a plain round robin gave the single MB O305 of a
                // depot the same share as the whole SD200 fleet.
                // A tour's vehicle for the day: the one `car_use` gives it, else one drawn now
                // and kept (a fleet number no other tour has, while there are any left).
                let (k, j) = match self.tour_vehicle.get(&h) {
                    Some(&kj) => kj,
                    None => {
                        let weights: Vec<usize> = vehicles.iter().map(|(_, n, _)| n.len().max(1)).collect();
                        let total: usize = weights.iter().sum();
                        let mut x = (h % total.max(1) as u64) as usize;
                        let mut k = 0usize;
                        for (i, w) in weights.iter().enumerate() {
                            if x < *w {
                                k = i;
                                break;
                            }
                            x -= w;
                        }
                        let nums = &vehicles[k].1;
                        let start = ((h >> 21) % nums.len().max(1) as u64) as usize;
                        let free = (0..nums.len())
                            .map(|o| (start + o) % nums.len())
                            .find(|&j| !self.used_numbers.contains(&(group.clone(), nums[j].number.trim().to_string())));
                        let j = free.unwrap_or(start);
                        if let Some(n) = nums.get(j) {
                            self.used_numbers.insert((group.clone(), n.number.trim().to_string()));
                        }
                        self.tour_vehicle.insert(h, (k, j));
                        (k, j)
                    }
                };
                let (ty, numbers, hof) = &vehicles[k.min(vehicles.len() - 1)];
                let numbers = numbers.get(j).cloned().into_iter().collect::<Vec<_>>();
                (ty.clone(), numbers, hof.clone())
            }
            _ => {
                // a plain [aigroup_2] flies/drives its own vehicles (the Tegel approach)
                let root = world.root.clone();
                let pool = self.pool(&root, world, &group);
                (
                    pool.get((h % pool.len().max(1) as u64) as usize).cloned()?,
                    Vec::new(),
                    None,
                )
            }
        };
        // A depot bus as Omsi.exe makes it (0x70a174): the fleet number of its ailists line;
        // the plate of that line, else - unless the bus's plates are free - the plate the bus
        // gives the number ([registration_list] / [registration_automatic]); and the repaint
        // that line names, else the model's own paint (the first repaint when the default
        // paint is "<nouse>"). Another tour's bus draws a repaint at random, as random
        // traffic does.
        let entry = numbers.first().cloned();
        let number = entry.as_ref().map(|e| {
            let plate = if !e.registration.trim().is_empty() {
                e.registration.clone()
            } else if ty.def.registration_mode != 1 {
                ty.def.plate_of_number(&e.number)
            } else {
                String::new()
            };
            (e.number.clone(), plate)
        });
        let scheme = if ty.paint_schemes.is_empty() {
            None
        } else if let Some(e) = &entry {
            ty.paint_schemes
                .iter()
                .position(|s| s.name.trim_end() == e.paint.trim_end())
                .or_else(|| (ty.def.default_paint.trim() == "<nouse>").then_some(0))
        } else {
            Some(
                ((h >> 42) % ty.paint_schemes.len().min(crate::traffic::AI_SCHEMES) as u64)
                    as usize,
            )
        };
        Some(Choice {
            ty,
            number,
            hof,
            scheme,
            train,
        })
    }

    /// The vehicle sets a choice is drawn with: the vehicle, its rear sections, a train's
    /// further cars.
    fn choice_sets(c: &Choice, traffic: &mut Traffic) -> Vec<(Arc<VehicleType>, Option<usize>)> {
        let mut out = vec![(c.ty.clone(), c.scheme)];
        for (t, _) in traffic.trailer_chain(&c.ty) {
            let s = c.scheme.filter(|i| *i < t.paint_schemes.len());
            out.push((t, s));
        }
        if let Some(cars) = &c.train {
            out.extend(cars.iter().skip(1).map(|(t, _)| (t.clone(), None)));
        }
        out
    }

    /// The vehicle sets of the trips on the road at `day_time` and of the departures of the
    /// next [`FLEET_AHEAD`] seconds.
    fn upcoming_sets(
        &mut self,
        world: &World,
        traffic: &mut Traffic,
        day_time: f64,
    ) -> Vec<(Arc<VehicleType>, Option<usize>)> {
        let mut out = Vec::new();
        let mut seen: HashSet<crate::scene::VehicleKey> = HashSet::new();
        let end = self
            .departures
            .partition_point(|d| d.time <= day_time - self.day_base + fleet_ahead());
        for i in 0..end {
            if self.is_player_tour(i) || !self.runs(i) {
                continue;
            }
            if self.dep_time(i) + self.times_of(i).duration < day_time {
                continue;
            }
            let Some(c) = self.choose(i, world) else {
                continue;
            };
            for (ty, scheme) in Self::choice_sets(&c, traffic) {
                if seen.insert((ty.def.path.clone(), scheme)) {
                    out.push((ty, scheme));
                }
            }
        }
        out
    }

    /// Keep the GPU's fleet to the vehicles of the next minutes: read the sets of the
    /// coming departures on the workers and upload them (one a frame) well before they are
    /// due, and let go of the sets nobody uses any more.
    fn fleet(
        &mut self,
        world: &World,
        traffic: &mut Traffic,
        renderer: &Renderer,
        scene: &mut Scene,
        day_time: f64,
    ) {
        let ready = self.fleet_ready.lock().pop();
        if let Some(key) = ready {
            if let Some(ty) = self.fleet_reading.remove(&key) {
                let t = std::time::Instant::now();
                world.precache_vehicle(renderer, scene, &ty, key.1);
                if omsi_cfg::env::var_os("OMSI_PROFILE").is_some() {
                    log::info!(
                        "timetable fleet: {} (scheme {:?}) uploaded ahead in {:.1} ms",
                        ty.def
                            .path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy(),
                        key.1,
                        t.elapsed().as_secs_f64() * 1000.0
                    );
                }
            }
        }
        if (day_time - self.fleet_check).abs() < 5.0 {
            return;
        }
        self.fleet_check = day_time;
        let sets = self.upcoming_sets(world, traffic, day_time);
        let keep: HashSet<crate::scene::VehicleKey> = sets
            .iter()
            .map(|(t, s)| (t.def.path.clone(), *s))
            .chain(self.fleet_reading.keys().cloned())
            .chain(traffic.random_sets().into_iter().map(|(t, s)| (t.def.path.clone(), s)))
            .collect();
        // (OMSI_FLEET_IDLE=<s> shortens the wait, for tests)
        let idle = omsi_cfg::env::var("OMSI_FLEET_IDLE")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .map(std::time::Duration::from_secs_f32)
            .unwrap_or(FLEET_IDLE);
        if world.trim_vehicle_sets(renderer, scene, &keep, idle) > 0 {
            crate::release_free_memory();
        }
        let prefetch = world.vehicle_prefetch(renderer);
        for (ty, scheme) in sets {
            let key = (ty.def.path.clone(), scheme);
            if self.fleet_reading.contains_key(&key) || world.has_vehicle_set(&key) {
                continue;
            }
            // two at a time: a set's repaints are compressed from pictures of tens of
            // megabytes (the rest follows at the next look, five seconds on)
            if self.fleet_reading.len() >= 2 {
                break;
            }
            self.fleet_reading.insert(key.clone(), ty.clone());
            let (p, ready) = (prefetch.clone(), self.fleet_ready.clone());
            // (off the frame's pool: see `threads`)
            crate::threads::background_pool().spawn(move || {
                p.prefetch(&ty, scheme);
                ready.lock().push(key);
            });
        }
    }

    /// The lanes a trip runs on (for the navigator): its track, else the station links
    /// between its stops, as far as the tiles have brought them - and whether that is all.
    pub fn trip_route(
        &self,
        world: &World,
        traffic: &Traffic,
        trip_name: &str,
    ) -> (Vec<usize>, bool) {
        let Some(trip) = self
            .data
            .trips
            .iter()
            .find(|x| x.name.eq_ignore_ascii_case(trip_name))
        else {
            return (Vec::new(), true);
        };
        let slots = self.slots(
            world,
            traffic,
            &self.steps_of(trip_name, &trip_stations(trip)).0,
            None,
        );
        let complete = !slots.contains(&Slot::Waiting);
        (
            slots
                .into_iter()
                .filter_map(|s| if let Slot::Lane(l) = s { Some(l) } else { None })
                .collect(),
            complete,
        )
    }

    /// The lanes a trip runs on in `net` - the navigator's network of the whole map, which
    /// has every tile's lanes whether loaded or not - chosen as `slots` chooses them (of a
    /// two-way path the direction that joins the lanes before and after).
    pub fn trip_route_in(&self, net: &omsi_sim::traffic::Network, trip_name: &str) -> Vec<usize> {
        let Some(trip) = self.data.trips.iter().find(|x| x.name.eq_ignore_ascii_case(trip_name)) else {
            return Vec::new();
        };
        let (steps, _) = self.steps_of(trip_name, &trip_stations(trip));
        let cands: Vec<Option<&Vec<usize>>> = steps.iter().map(|st| st.key.and_then(|k| net.by_key.get(&k)).filter(|c| !c.is_empty())).collect();
        let mut out: Vec<Slot> = Vec::with_capacity(steps.len());
        let mut last: Option<usize> = None;
        for (i, c) in cands.iter().enumerate() {
            let Some(c) = c else {
                out.push(Slot::Absent);
                continue;
            };
            let next = cands[i + 1..].iter().find_map(|x| *x);
            let score = |l: usize| -> f64 {
                let mut s = 0.0;
                if let Some(prev) = last {
                    s += (net.lanes[l].start() - net.lanes[prev].end()).length();
                }
                if let Some(next) = next {
                    let end = net.lanes[l].end();
                    s += next.iter().map(|&n| (net.lanes[n].start() - end).length()).fold(f64::MAX, f64::min);
                }
                s
            };
            let best = c.iter().copied().min_by(|a, b| score(*a).total_cmp(&score(*b))).unwrap();
            out.push(Slot::Lane(best));
            last = Some(best);
        }
        skip_detours(net, &mut out);
        out.into_iter().filter_map(|s| if let Slot::Lane(l) = s { Some(l) } else { None }).collect()
    }

    /// `OMSI_CHECK_TRIPS=1`: build the route of every trip on the loaded lanes and say where
    /// consecutive lanes do not join - a gap the bus would jump, or a lane taken the wrong
    /// way round (its end, not its start, lies where the lane before ends), which sends a
    /// bus into the oncoming traffic. Only trips whose route is wholly loaded are judged.
    pub fn check_routes(&self, world: &World, traffic: &mut Traffic) {
        for trip in &self.data.trips {
            let (steps, _) = self.steps_of(&trip.name, &trip_stations(trip));
            Self::add_twins(traffic, &steps);
            let lanes: Vec<usize> = self
                .slots(world, traffic, &steps, None)
                .iter()
                .filter_map(|s| if let Slot::Lane(l) = s { Some(*l) } else { None })
                .collect();
            Self::add_connectors(traffic, &lanes);
        }
        let traffic = &*traffic;
        let net = &traffic.net;
        let (mut trips, mut joints, mut linked, mut changes, mut gaps, mut wrong, mut partial) =
            (0, 0, 0, 0, 0, 0, 0);
        let mut bad_length = 0;
        for trip in &self.data.trips {
            let stations = trip_stations(trip);
            let (steps, _) = self.steps_of(&trip.name, &stations);
            let slots = self.slots(world, traffic, &steps, None);
            if slots.contains(&Slot::Waiting) || steps.is_empty() {
                if partial < 5 {
                    let k = slots.iter().position(|s| *s == Slot::Waiting).unwrap_or(0);
                    log::info!(
                        "check trips: {}: {} steps, {} on tiles not loaded, first {:?} (tile loaded {}, in the map {})",
                        trip.name,
                        steps.len(),
                        slots.iter().filter(|s| **s == Slot::Waiting).count(),
                        steps.get(k).and_then(|s| s.key),
                        steps.get(k).and_then(|s| s.key).map(|key| traffic.lane_tiles.contains(&key.tile)).unwrap_or(false),
                        steps.get(k).and_then(|s| s.key).map(|key| world.has_tile(key.tile)).unwrap_or(false),
                    );
                }
                partial += 1;
                continue;
            }
            trips += 1;
            // `OMSI_CHECK_TRIPS=<trip name>`: every step of that trip
            if omsi_cfg::env::var("OMSI_CHECK_TRIPS").map(|v| v.eq_ignore_ascii_case(&trip.name)).unwrap_or(false) {
                for (k, (st, sl)) in steps.iter().zip(&slots).enumerate() {
                    let cands: Vec<String> = st
                        .key
                        .and_then(|key| net.by_key.get(&key))
                        .map(|c| {
                            c.iter()
                                .map(|&l| {
                                    let x = &net.lanes[l];
                                    format!("{l}{} ({:.1},{:.1})->({:.1},{:.1}) {:.1} m", if x.reversed { "r" } else { "" }, x.start().x, x.start().y, x.end().x, x.end().y, x.length())
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    log::info!("check trips: {} step {k} leg {} {:?} ({:.1} m): {:?} of {:?}", trip.name, st.leg, st.key, st.length, sl, cands);
                }
            }
            // the lane a step names should be as long as the file says its path is
            for (st, sl) in steps.iter().zip(&slots) {
                if let Slot::Lane(l) = sl {
                    let len = net.lanes[*l].length() as f64;
                    if st.length > 0.5 && (len - st.length).abs() > 1.0 + 0.05 * st.length {
                        if bad_length < 12 {
                            log::info!(
                                "check trips: {}: lane {} {:?} is {len:.1} m long, the file says {:.1} m",
                                trip.name, l, net.lanes[*l].key, st.length
                            );
                        }
                        bad_length += 1;
                    }
                }
            }
            let lanes: Vec<usize> = slots
                .iter()
                .filter_map(|s| if let Slot::Lane(l) = s { Some(*l) } else { None })
                .collect();
            let lanes = bridge_gaps(net, &lanes).0;
            let mut shown = 0;
            for (k, w) in lanes.windows(2).enumerate() {
                let (a, b) = (&net.lanes[w[0]], &net.lanes[w[1]]);
                joints += 1;
                if a.next.contains(&w[1]) {
                    linked += 1;
                    continue;
                }
                if net.parallel(w[0], w[1]) {
                    changes += 1;
                    continue;
                }
                let gap = (b.start() - a.end()).truncate().length();
                let backwards = (b.end() - a.end()).truncate().length();
                let is_wrong = backwards + 1.0 < gap && backwards < 3.0;
                if is_wrong {
                    wrong += 1;
                } else if gap > 2.0 {
                    gaps += 1;
                } else {
                    linked += 1;
                    continue;
                }
                if shown < 6 {
                    shown += 1;
                    log::info!(
                        "check trips: {}: step {k}: lane {} {:?} rev {} -> {} {:?} rev {}: {} (gap {gap:.1} m, to its end {backwards:.1} m) at ({:.0}, {:.0})",
                        trip.name, w[0], a.key, a.reversed, w[1], b.key, b.reversed,
                        if is_wrong { "WRONG WAY" } else { "gap" }, a.end().x, a.end().y
                    );
                }
            }
        }
        log::info!("check trips: {trips} trips on loaded lanes ({partial} not wholly loaded): {joints} joints, {linked} joined, {changes} lane changes, {gaps} gaps, {wrong} taken the wrong way round; {bad_length} lanes not as long as the file says");
    }

    /// Vehicles of a plain `[aigroup_2]`, loaded on first use and kept.
    fn pool(&mut self, root: &Path, world: &World, group: &str) -> &[Arc<VehicleType>] {
        if !self.pools.contains_key(group) {
            let mut out = Vec::new();
            for g in world
                .ailists
                .groups
                .iter()
                .filter(|g| !g.is_depot && g.name.to_ascii_lowercase() == group)
            {
                for v in &g.vehicles {
                    if v.file.to_ascii_lowercase().ends_with(".zug") {
                        continue;
                    }
                    let path = omsi_cfg::resolve_path(root, &v.file);
                    match VehicleType::load_ai(root, &path) {
                        Ok(t) => out.push(Arc::new(t)),
                        Err(e) => log::warn!("AI group '{}' vehicle {}: {e}", g.name, v.file),
                    }
                }
            }
            if !out.is_empty() {
                log::info!(
                    "AI group '{group}': {} vehicles loaded for its timetable trips",
                    out.len()
                );
            }
            self.pools.insert(group.to_string(), out);
        }
        self.pools.get(group).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// How many due departures are still waiting to be put on the road.
    /// Omsi.exe's station targets (0x61cb18): per bus stop, the stops the timetable's trips
    /// go on to from there, each with the termini of the trips that do. A passenger waiting
    /// at the stop wants one of these targets and boards a bus whose terminus is among its
    /// termini (0x61c33c); the names compare exactly.
    pub fn stop_targets(&self) -> HashMap<i64, Vec<(String, HashSet<String>)>> {
        let names = self.stop_names();
        let name_of = |id: i64| names.get(&id).cloned().unwrap_or_else(|| id.to_string());
        station_targets(self.data.trips.iter().map(|t| (trip_stations(t), t.terminus.trim().to_string())), name_of)
    }

    /// The name each bus stop object has in the timetable (`Busstops.cfg`, the first entry
    /// of an object id): what [`Schedule::stop_targets`] calls it. The map object's own
    /// label can read otherwise (renamed in the editor, another code page than the tiles').
    pub fn stop_names(&self) -> HashMap<i64, String> {
        let mut names = HashMap::new();
        for b in &self.data.bus_stops {
            names.entry(b.object_id).or_insert_with(|| b.name.trim().to_string());
        }
        names
    }

    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Spawn buses whose departure time has come (or passed within `window` seconds), and
    /// the layover buses of the next quarter of an hour.
    pub fn tick(
        &mut self,
        world: &World,
        traffic: &mut Traffic,
        renderer: &Renderer,
        scene: &mut Scene,
        day_time: f64,
        window: f64,
    ) {
        // (a LAN client draws the host's timetable buses)
        if traffic.is_mirror() {
            return;
        }
        self.roll_day(day_time);
        // the time of day of today's timetable
        let tod = day_time - self.day_base;
        self.last_tod = tod;
        if std::mem::take(&mut self.purge_player_tour) {
            let gone: Vec<u64> = self
                .car_departure
                .iter()
                .filter(|(_, i)| self.is_player_tour(**i))
                .map(|(id, _)| *id)
                .collect();
            for id in gone {
                self.car_departure.remove(&id);
                self.running.retain(|r| r.car != id);
                if traffic.remove_car(world, renderer, scene, id) {
                    log::info!("timetable: bus {id} of the player's tour taken off the road");
                }
            }
        }
        let loading = window > 60.0;
        let due: Vec<usize> = self
            .departures
            .iter()
            .enumerate()
            .filter(|(i, d)| !d.spawned && d.time <= tod && d.time > tod - window && self.runs(*i))
            .map(|(i, _)| i)
            .collect();
        for i in due {
            self.departures[i].spawned = true;
            // the tour's bus is still on its way here: it takes the trip on when it arrives
            if self.tour_prev[i].and_then(|k| self.tour_bus(k, traffic)).is_some() {
                self.awaiting.insert(i);
                continue;
            }
            self.pending.push_back(i);
            if loading {
                self.startup.insert(i);
            }
        }
        // Buses on their layover: a trip that leaves within the next quarter of an hour,
        // whose tour's previous trip is already over, stands at its first stop with the
        // doors shut until its departure. Without this a map with one bus per line
        // showed no bus at all for most of the hour - it only existed while driving.
        let mut early: Vec<usize> = Vec::new();
        // departures are sorted by time: only the ones in the next quarter of an hour
        let start = self.departures.partition_point(|d| d.time <= tod);
        for i in start..self.departures.len() {
            let d = &self.departures[i];
            if d.time > tod + LAYOVER {
                break;
            }
            if d.spawned || self.later_layover.contains(&i) || !self.runs(i) {
                continue;
            }
            // only a trip with stops has a first stop to wait at: a flight (TXL.ttl, every
            // ten minutes) would take off a quarter of an hour early
            let Some(first) = trip_stations(&self.data.trips[d.trip]).first().copied() else {
                continue;
            };
            if d.time > tod + LAYOVER_SHARED {
                let shared = match self.shared_stand.get(&first) {
                    Some(&v) => v,
                    None => {
                        let positions = world.object_positions.lock();
                        let Some(&(here, _)) = positions.get(&first) else {
                            continue;
                        };
                        let v = self.served.contains(&first)
                            || self.served.iter().any(|sid| {
                            positions
                                .get(sid)
                                .map(|p| (p.0 - here).length() < 30.0)
                                .unwrap_or(false)
                        });
                        if omsi_cfg::env::var_os("OMSI_DEBUG_TRAFFIC").is_some() {
                            let nearest = self
                                .served
                                .iter()
                                .filter_map(|sid| {
                                    positions.get(sid).map(|p| ((p.0 - here).length(), *sid))
                                })
                                .fold((f64::MAX, 0), |a, b| if b.0 < a.0 { b } else { a });
                            log::info!("layover stand {first} at ({:.0}, {:.0}): shared {v}, nearest served station {} at {:.0} m", here.x, here.y, nearest.1, nearest.0);
                        }
                        drop(positions);
                        self.shared_stand.insert(first, v);
                        v
                    }
                };
                if shared {
                    continue;
                }
            }
            let prev_running = self.tour_prev[i]
                .map(|k| {
                    self.dep_time(k) + self.times_of(k).duration >= day_time
                        || self.tour_bus(k, traffic).is_some()
                })
                .unwrap_or(false);
            if !prev_running {
                early.push(i);
            }
        }
        for i in early {
            self.departures[i].spawned = true;
            self.pending.push_back(i);
            if loading {
                self.startup.insert(i);
            }
        }
        // buses whose ground was unloaded under them wait for it to come back
        for id in std::mem::take(&mut traffic.removed_scheduled) {
            if let Some(i) = self.car_departure.remove(&id) {
                log::debug!("departure {i}: its bus left the loaded tiles, waiting for them");
                self.waiting.push(i);
            }
        }
        if self.car_departure.len() > 64 + traffic.cars.len() * 2 {
            let alive: std::collections::HashSet<u64> = traffic.cars.iter().map(|c| c.id).collect();
            self.car_departure.retain(|id, _| alive.contains(id));
        }
        // tiles brought lanes, or half a minute went by: the waiting departures may be on
        // loaded ground now, and the routes that stopped short may go on
        let grew = traffic.lanes_generation != self.seen_generation;
        if grew || day_time - self.last_retry >= 30.0 || day_time < self.last_retry {
            self.last_retry = day_time;
            for i in std::mem::take(&mut self.waiting) {
                if !self.pending.contains(&i) {
                    self.pending.push_back(i);
                }
            }
            self.retry_at.clear();
        } else if !self.retry_at.is_empty() {
            // the ones whose vehicle has just reached the loaded part of its way
            let due: Vec<usize> = self
                .waiting
                .iter()
                .copied()
                .filter(|i| {
                    self.retry_at
                        .get(i)
                        .map(|t| *t <= day_time)
                        .unwrap_or(false)
                })
                .collect();
            if !due.is_empty() {
                self.waiting.retain(|i| !due.contains(i));
                for i in due {
                    self.retry_at.remove(&i);
                    if !self.pending.contains(&i) {
                        // ahead of the others: it is due now
                        self.pending.push_front(i);
                    }
                }
            }
        }
        if grew {
            self.seen_generation = traffic.lanes_generation;
            self.carry_on(world, traffic);
        }
        self.fleet(world, traffic, renderer, scene, day_time);
        self.tour_handover(world, traffic, renderer, scene, day_time);
        // a handful per call: spawning a bus builds its meshes, and a whole rush hour at
        // once is a frame that lasts seconds (a departure that has to wait costs little)
        let (mut spawned, mut tried) = (0, 0);
        while spawned < if loading { 3 } else { 1 } && tried < 24 {
            let Some(i) = self.pending.pop_front() else {
                break;
            };
            tried += 1;
            match self.spawn_departure(i, world, traffic, renderer, scene, day_time, None) {
                Placed::Spawned => spawned += 1,
                Placed::Wait => self.waiting.push(i),
                Placed::Busy => {
                    // a vehicle stands where the bus would appear (the player's bus may stand
                    // there for its whole layover): again in a few seconds, without keeping
                    // the traffic on its quick spawning pace meanwhile
                    self.retry_at.insert(i, day_time + 3.0);
                    self.waiting.push(i);
                }
                Placed::Drop => {}
            }
        }
    }

    /// Carry the routes of the running trips on over the lanes the network gained.
    fn carry_on(&mut self, world: &World, traffic: &mut Traffic) {
        let mut keep = Vec::new();
        for mut run in std::mem::take(&mut self.running) {
            let Some(ci) = traffic.cars.iter().position(|c| c.id == run.car) else {
                continue;
            };
            let last = traffic.cars[ci].state.route.last().copied();
            Self::add_twins(traffic, &run.steps[run.next.saturating_sub(1)..]);
            let slots = self.slots(world, traffic, &run.steps[run.next..], last);
            let n = slots
                .iter()
                .position(|s| *s == Slot::Waiting)
                .unwrap_or(slots.len());
            let lanes: Vec<usize> = slots[..n]
                .iter()
                .filter_map(|s| {
                    if let Slot::Lane(l) = s {
                        Some(*l)
                    } else {
                        None
                    }
                })
                .collect();
            // (bridged from the end of what the bus has)
            let lanes = match last {
                Some(l) if !lanes.is_empty() => {
                    let with: Vec<usize> = std::iter::once(l).chain(lanes.iter().copied()).collect();
                    Self::add_connectors(traffic, &with);
                    bridge_gaps(&traffic.net, &with).0[1..].to_vec()
                }
                _ => {
                    Self::add_connectors(traffic, &lanes);
                    bridge_gaps(&traffic.net, &lanes).0
                }
            };
            if !lanes.is_empty() {
                let base = traffic.cars[ci].state.route.len();
                let mut stops = Vec::new();
                let mut from = 0;
                for (si, (sid, t_dep)) in run.stations.iter().enumerate() {
                    if run.served[si] {
                        continue;
                    }
                    let Some((pos, _)) = world.object_positions.lock().get(sid).copied() else {
                        continue;
                    };
                    if let Some((ri, ss, lat)) =
                        project_stop(&traffic.net, &lanes, pos, Some(STOP_REACH), from)
                    {
                        from = ri;
                        stops.push((base + ri, ss, bay_offset(lat), *t_dep, *sid, world.stop_side(*sid)));
                        run.served[si] = true;
                    }
                }
                let (ty, rail) = (traffic.cars[ci].vehicle.ty.clone(), traffic.cars[ci].is_rail());
                place_stops(&traffic.net, &lanes, base, &mut stops, &ty, rail);
                stops.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
                log::debug!(
                    "scheduled bus {}: route carried on by {} lanes, {} more stops",
                    run.car,
                    lanes.len(),
                    stops.len()
                );
                let car = &mut traffic.cars[ci];
                car.state.route.extend(lanes);
                if let Some(b) = car.bus.as_mut() {
                    b.stops.extend(stops.into_iter().map(crate::bus_service::Stop::from_tuple));
                }
                // (it may have stood waiting at the end of what it had)
                car.state.planned_next = None;
                car.state.plan_next(&traffic.net);
            }
            run.next += n;
            if run.next < run.steps.len() {
                keep.push(run);
            } else {
                if let Some(b) = traffic.cars[ci].bus.as_mut() {
                    b.route_open = false;
                }
            }
        }
        self.running = keep;
    }

    /// Put departure `i` on the road where its bus is at `day_time`: on the part of the route
    /// the loaded tiles have, which is carried on as more tiles come.
    ///
    /// With `onto`, the timetable bus of that index (the tour's bus, at the end of its
    /// previous trip) takes the trip on instead of a new one.
    #[allow(clippy::too_many_arguments)]
    fn spawn_departure(
        &mut self,
        i: usize,
        world: &World,
        traffic: &mut Traffic,
        renderer: &Renderer,
        scene: &mut Scene,
        day_time: f64,
        onto: Option<usize>,
    ) -> Placed {
        let profile = omsi_cfg::env::var_os("OMSI_PROFILE").is_some();
        let t_spawn = std::time::Instant::now();
        let trip = &self.data.trips[self.departures[i].trip];
        let trip_name = trip.name.clone();
        // (the [station] records of a type-1 trip as well: Novi Sad's buses have no others,
        // and without them they drove past every stop)
        let stations = trip_stations(trip);
        // the timetable's times at the stations (see `TripTimes`)
        let departure = self.dep_time(i);
        let tt = self.times_of(i).clone();
        let duration = tt.duration;
        if day_time - departure > duration && onto.is_none() {
            return Placed::Drop; // already arrived
        }
        let arrive: Vec<f64> = tt.stations.iter().map(|s| departure + s.0).collect();
        let leave: Vec<f64> = tt.stations.iter().map(|s| departure + s.1).collect();
        let (steps, track) = self.steps_of(&trip_name, &stations);
        Self::add_twins(traffic, &steps);
        let slots = self.slots(world, traffic, &steps, None);
        if !slots.iter().any(|s| matches!(s, Slot::Lane(_))) && !slots.contains(&Slot::Waiting) {
            log::debug!("trip {trip_name}: no route");
            return Placed::Drop;
        }
        // the leg the bus is on now, and how far along it (a layover bus stands at the start)
        let legs = if track {
            1
        } else {
            stations.len().saturating_sub(1).max(1)
        };
        let leg_time = |k: usize| {
            if track {
                (departure, departure + duration)
            } else {
                (
                    leave[k],
                    arrive.get(k + 1).copied().unwrap_or(departure + duration),
                )
            }
        };
        // (the tour's own bus taking the trip on starts it at its first stop, however late)
        let leg = (0..legs)
            .rev()
            .find(|&k| leg_time(k).0 <= day_time)
            .filter(|_| onto.is_none())
            .unwrap_or(0);
        let (t0, t1) = leg_time(leg);
        let frac = if day_time <= departure || onto.is_some() {
            0.0
        } else {
            ((day_time - t0) / (t1 - t0).max(1e-3)).clamp(0.0, 1.0)
        };
        // lengths of the steps: a step still to come counts as long as an average one
        let net = &traffic.net;
        let known: Vec<f64> = slots
            .iter()
            .filter_map(|s| {
                if let Slot::Lane(l) = s {
                    Some(net.lanes[*l].length() as f64)
                } else {
                    None
                }
            })
            .collect();
        let average = if known.is_empty() {
            40.0
        } else {
            known.iter().sum::<f64>() / known.len() as f64
        };
        let est: Vec<f64> = slots
            .iter()
            .map(|slot| match slot {
                Slot::Lane(l) => net.lanes[*l].length() as f64,
                Slot::Waiting => average,
                Slot::Absent => 0.0,
            })
            .collect();
        let Some((at, offset)) = step_at(&steps, &slots, &est, leg, frac) else {
            return Placed::Drop;
        };
        if slots[at] == Slot::Waiting {
            // when it reaches the next loaded step, at the pace of its leg
            let in_leg = |k: usize| track || steps[k].leg == leg;
            let leg_len: f64 = (0..steps.len())
                .filter(|&k| in_leg(k))
                .map(|k| est[k])
                .sum();
            let rate = (t1 - t0).max(0.0) / leg_len.max(1e-3);
            let base = day_time.max(departure);
            let retry = match (at + 1..slots.len()).find(|&k| matches!(slots[k], Slot::Lane(_))) {
                Some(k) if in_leg(k) => {
                    let ahead =
                        (est[at] - offset).max(0.0) + (at + 1..k).map(|j| est[j]).sum::<f64>();
                    base + ahead * rate
                }
                // on a later leg (or nowhere): look again when this leg is over
                _ => t1.max(base),
            };
            log::debug!(
                "trip {trip_name}: the bus is on a tile that is not loaded (again at {:.2} min)",
                retry / 60.0
            );
            self.retry_at.insert(i, retry);
            return Placed::Wait;
        }
        // the part of the route around the bus that the network has
        let (start, end) = section_around(&slots, at);
        let whole = start == 0 && end == slots.len();
        let lane_of = |s: &Slot| {
            if let Slot::Lane(l) = s {
                Some(*l)
            } else {
                None
            }
        };
        let section: Vec<usize> = slots[start..end].iter().filter_map(lane_of).collect();
        let start_index = slots[start..at].iter().filter_map(lane_of).count();
        Self::add_connectors(traffic, &section);
        let net = &traffic.net;
        let (section, index) = bridge_gaps(net, &section);
        let start_index = index[start_index.min(index.len() - 1)];
        let mut s = offset.min(net.lanes[section[start_index]].length() as f64) as f32;
        // stations → stop points on that part of the route
        let reach = if whole { None } else { Some(STOP_REACH) };
        let mut served = vec![false; stations.len()];
        let mut stops = Vec::new();
        let mut from = 0;
        for (si, sid) in stations.iter().enumerate() {
            // a station the trip runs through is none of its stops
            if !tt.stops[si] {
                served[si] = true;
                continue;
            }
            // the stations of the legs behind the bus are passed
            if !track && (si < leg || (si == leg && frac > 0.0)) {
                served[si] = true;
            }
            let found = world.object_positions.lock().get(sid).copied();
            match found {
                Some((pos, _)) => match project_stop(net, &section, pos, reach, from) {
                    Some((ri, ss, lat)) => {
                        from = ri;
                        served[si] = true;
                        stops.push((ri, ss, bay_offset(lat), leave[si], *sid, world.stop_side(*sid)));
                    }
                    None => log::debug!("station {sid}: not near the route"),
                },
                None => log::debug!("station {sid}: object not in the map"),
            }
        }
        stops.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
        if omsi_cfg::env::var_os("OMSI_DEBUG_TRAFFIC").is_some() {
            let len: f32 = section.iter().map(|&l| net.lanes[l].length()).sum();
            log::info!("trip {trip_name}: {} stations {:?}, route {} of {} steps ({} lanes, {len:.0} m) from step {start}, bus on step {at} (leg {leg}, {:.0} %), stops {:?}", stations.len(), stations, end - start, steps.len(), section.len(), frac * 100.0, stops);
        }
        let t_route = t_spawn.elapsed();
        if let Some(ci) = onto {
            // a train whose next trip runs the other way (its `[trainreverse]` is not how the
            // train stands) is turned round where it stands, as Omsi.exe turns it when the
            // trip begins (0x613a98): its last car leads, on the way back. (It drove off
            // along the siding instead - past the end of the track - and another train
            // appeared for the trip.)
            let reverse = self.data.trips[self.departures[i].trip].train_reverse;
            if traffic.cars[ci].is_rail() && traffic.cars[ci].consist_reversed != reverse {
                let c = &traffic.cars[ci];
                let tail = c.vehicle.trailers.last().map(|t| t.position).unwrap_or(c.vehicle.position);
                let net = &traffic.net;
                let found = section
                    .iter()
                    .enumerate()
                    .take(24)
                    .filter_map(|(k, &l)| net.lanes[l].nearest_point(tail).map(|(s, d)| (k, l, s, d)))
                    .min_by(|a, b| a.3.total_cmp(&b.3));
                match found {
                    Some((k, l, s, d)) if d < 2.5 => {
                        traffic.turn_train(world, renderer, scene, ci, l, s, &section[..k], reverse);
                    }
                    _ => {
                        if omsi_cfg::env::var_os("OMSI_DEBUG_TRAFFIC").is_some() {
                            log::info!("trip {trip_name}: train {} would turn round, but its last car at ({:.1}, {:.1}) is not on the trip's way (nearest {:?}); its front at ({:.1}, {:.1}) on lane {}", c.id, tail.x, tail.y, found.map(|f| (f.0, f.1, f.2, f.3)), c.vehicle.position.x, c.vehicle.position.y, c.state.lane);
                            for &l in section.iter().take(4).chain(std::iter::once(&c.state.lane)) {
                                let ln = &net.lanes[l];
                                log::info!("  lane {l} {:?} len {:.1} ({:.1}, {:.1}) -> ({:.1}, {:.1}) next {:?} rev {}", ln.key, ln.length(), ln.start().x, ln.start().y, ln.end().x, ln.end().y, ln.next, ln.reversed);
                            }
                        }
                    }
                }
            }
            let (ty, rail) = (traffic.cars[ci].vehicle.ty.clone(), traffic.cars[ci].is_rail());
            place_stops(&traffic.net, &section, 0, &mut stops, &ty, rail);
            // the tour's bus that has just finished its trip takes this one on from where
            // it stands: the section itself when it stands on it, else the shortest way
            // from its lane onto one of the section's first lanes (round a terminal loop)
            let (lane0, s0) = (traffic.cars[ci].state.lane, traffic.cars[ci].state.s);
            let net = &traffic.net;
            let (prefix, from) = match tour_entry(&section, lane0) {
                Some(r) => (Vec::new(), r),
                None => {
                    let mut best: Option<(f32, Vec<usize>, usize)> = None;
                    for t in 0..section.len().min(TOUR_ENTRY_LANES) {
                        if let Some(p) = net.shortest_path(lane0, section[t]) {
                            let len = p[..p.len() - 1].iter().map(|&l| net.lanes[l].length()).sum::<f32>() - s0;
                            if len < 400.0 && best.as_ref().map(|b| len < b.0).unwrap_or(true) {
                                best = Some((len, p, t));
                            }
                        }
                    }
                    match best {
                        Some((_, p, t)) => (p[..p.len() - 1].to_vec(), t),
                        None => {
                            log::debug!("trip {trip_name}: the tour's bus has no way from where it stands");
                            if omsi_cfg::env::var_os("OMSI_DEBUG_TRAFFIC").is_some() {
                                let ln = &net.lanes[lane0];
                                log::info!("trip {trip_name}: tour bus on lane {lane0} {:?} at s {s0:.1} of {:.1}, ({:.1}, {:.1}) -> ({:.1}, {:.1}), next {:?}", ln.key, ln.length(), ln.start().x, ln.start().y, ln.end().x, ln.end().y, ln.next);
                                for &l in section.iter().take(4) {
                                    let ln = &net.lanes[l];
                                    log::info!("  trip lane {l} {:?} len {:.1} ({:.1}, {:.1}) -> ({:.1}, {:.1}) prev? next {:?}", ln.key, ln.length(), ln.start().x, ln.start().y, ln.end().x, ln.end().y, ln.next);
                                }
                            }
                            return Placed::Drop;
                        }
                    }
                }
            };
            let route: Vec<usize> = prefix.iter().copied().chain(section[from..].iter().copied()).collect();
            let shift = prefix.len() as isize - from as isize;
            // the stops from the bus on; one just behind it on its lane is where it stands
            let stops: Vec<(usize, f32, f32, f64, i64, f32)> = stops
                .into_iter()
                .filter(|st| st.0 >= from)
                .filter_map(|(ri, ss, lat, t, id, side)| {
                    let nri = (ri as isize + shift) as usize;
                    if nri == 0 && ss <= s0 + 0.3 {
                        (s0 - ss < 25.0).then_some((0, s0 + 0.3, 0.0, t, id, side))
                    } else {
                        Some((nri, ss, lat, t, id, side))
                    }
                })
                .collect();
            let layover = departure > day_time;
            let n_stops = stops.len();
            traffic.reroute(ci, route, s0, stops, layover);
            let line = self.display_line(i);
            let terminus = self.data.trips[self.departures[i].trip].terminus.clone();
            let names = self.trip_stop_names(self.departures[i].trip);
            let last_stop = trip_stations(&self.data.trips[self.departures[i].trip]).last().copied();
            let (always, early) = self.special_stops(i);
            let car = &mut traffic.cars[ci];
            if let Some(k) = car.vehicle.ty.program.str_var("Linie") {
                car.vehicle.state.str_vars[k as usize] = line.clone();
            }
            let hof = car.vehicle.host.hof.clone();
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            set_ai_destination(&mut car.vehicle, hof.as_deref(), &line, &terminus, &names);
            if let Some(b) = car.bus.as_mut() {
                b.route_open = end < slots.len();
                b.terminus = terminus.clone();
                b.last_stop = last_stop;
                b.always = always;
                b.serve_early = early;
            }
            let id = car.id;
            self.car_departure.insert(id, i);
            self.running.retain(|r| r.car != id);
            if end < slots.len() {
                self.running.push(RunningTrip {
                    car: id,
                    steps,
                    next: end,
                    stations: stations.iter().copied().zip(leave.iter().copied()).collect(),
                    served,
                });
            }
            log::info!(
                "scheduled bus {id}: line {line} tour {} goes on with trip {trip_name} to {} at {:.1} min (leaves {:.1} min), {n_stops} stops{}",
                self.departures[i].tour,
                terminus.trim(),
                day_time / 60.0,
                departure / 60.0,
                if prefix.is_empty() { String::new() } else { format!(", {} lanes to its first stop", prefix.len()) }
            );
            return Placed::Spawned;
        }
        let Some(Choice {
                     ty,
                     number,
                     hof,
                     scheme,
                     train,
                 }) = self.choose(i, world)
        else {
            log::warn!(
                "trip {trip_name}: no vehicles for AI group '{}'",
                self.departures[i].ai_group
            );
            return Placed::Drop;
        };
        self.next_number += 1;
        let rail = traffic.net.lanes[section[start_index]].kind == omsi_sim::traffic::LaneKind::Rail;
        // every further car of the train with the cars of its unit, as Omsi.exe creates
        // each car of a `.zug` (the first has its own with `create_car`): the ones before it
        // (towards the front of the train), the car, the ones behind it
        let rest: Option<Vec<(Arc<VehicleType>, bool)>> = train.as_ref().map(|cars| {
            let mut rest = Vec::new();
            for (t, rev) in &cars[1..] {
                let mut front = traffic.coupled_chain(t, *rev, false);
                front.reverse();
                rest.extend(front);
                rest.push((t.clone(), *rev));
                rest.extend(traffic.coupled_chain(t, *rev, true));
            }
            rest
        });
        // a trip that runs the train turned round (`[trainreverse]`): its last car leads
        let turned: Option<Vec<(Arc<VehicleType>, bool)>> = (self.data.trips[self.departures[i].trip].train_reverse
            && rail)
            .then(|| {
                let mut all = vec![(ty.clone(), false)];
                all.extend(traffic.trailer_chain(&ty));
                all.extend(rest.clone().unwrap_or_default());
                all.into_iter().rev().map(|(t, r)| (t, !r)).collect()
            });
        // where the one that leads comes to rest at a station
        let lead_ty = turned.as_ref().map(|t| t[0].0.clone()).unwrap_or_else(|| ty.clone());
        place_stops(&traffic.net, &section, 0, &mut stops, &lead_ty, rail);
        log::debug!("spawn trip {trip_name}: departure {:.2} min, now {:.2} min, leg {leg} at {:.0} %, step {at} of {}, start {s:.0} m into its lane", departure / 60.0, day_time / 60.0, frac * 100.0, steps.len());
        // the bus starts on its step's lane; the stops behind it are dropped
        let mut start_index = start_index;
        while start_index + 1 < section.len()
            && s > traffic.net.lanes[section[start_index]].length()
        {
            s -= traffic.net.lanes[section[start_index]].length();
            start_index += 1;
        }
        // a bus that would start a few metres short of its next stop stands at it (half a
        // metre short, so that it is served): starting before it, it had to pull over into
        // the stop - often a lane over - in less than its own length
        if let Some(&(ri, ss, _, _, _, _)) = stops
            .iter()
            .find(|st| st.0 > start_index || (st.0 == start_index && st.1 > s))
        {
            let mut d = ss - s;
            for k in start_index..ri {
                if !traffic.net.parallel(section[k], section[k + 1]) {
                    d += traffic.net.lanes[section[k]].length();
                }
            }
            if d < 25.0 {
                start_index = ri;
                s = (ss - 0.5).max(0.0);
            }
        }
        let at_pos = traffic.net.lanes[section[start_index]].at(s).0;
        // lanes stay in the network when their tile is unloaded: nothing is put on ground
        // that is not there (the departure waits for its tile)
        if !track_is_air(traffic, section[start_index]) && !world.has_ground(at_pos.x, at_pos.y) {
            log::debug!("trip {trip_name}: the bus would stand on an unloaded tile");
            return Placed::Wait;
        }
        // not into a vehicle that happens to be there (the player's bus at its stop, a car),
        // nor just in front of one driving up to that place: try again in a moment
        let at_heading = traffic.net.lanes[section[start_index]].at(s).1 as f64;
        if traffic.blocked(&ty, at_pos, at_heading) || !traffic.spawn_clear(&ty, at_pos, at_heading)
        {
            log::debug!("trip {trip_name}: a vehicle stands where the bus would appear");
            return Placed::Busy;
        }
        // A bus on its layover waits at the stand only when no other bus stands there or
        // pulls in; otherwise it comes at its departure time. (A stand shared by four lines
        // had five buses queueing in the road for a quarter of an hour, and every bus
        // serving the stop and every car behind them waiting as well.)
        if departure > day_time + 30.0
            && traffic
            .cars
            .iter()
            .any(|c| c.is_bus() && !c.gone && (c.vehicle.position - at_pos).length() < 50.0)
        {
            log::debug!(
                "trip {trip_name}: its stand is taken, the bus comes at its departure time"
            );
            self.departures[i].spawned = false;
            self.startup.remove(&i);
            self.later_layover.insert(i);
            return Placed::Drop;
        }
        // nobody may see a bus appear (except while the map loads)
        // (the map-loading exemption holds only while the world is being built: a departure
        // of that moment that had to wait popped up in plain view minutes later)
        if !(self.startup.contains(&i) && traffic.loading_phase()) && !traffic.may_appear(world, at_pos) {
            log::debug!("trip {trip_name}: the bus would appear in view");
            return Placed::Busy;
        }
        self.startup.remove(&i);
        let stops: Vec<(usize, f32, f32, f64, i64, f32)> = stops
            .into_iter()
            .filter(|(ri, ss, _, _, _, _)| *ri > start_index || (*ri == start_index && *ss > s))
            .map(|(ri, ss, lat, t, id, side)| (ri - start_index, ss, lat, t, id, side))
            .collect();
        let route: Vec<usize> = section[start_index..].to_vec();
        // the trip's own line (" 5"), which is what the displays show; the timetable line's
        // name ("5 & 5N") only groups the tours
        let trip_line = self.data.trips[self.departures[i].trip]
            .line
            .trim()
            .to_string();
        let line = if trip_line.is_empty() {
            self.departures[i].line.clone()
        } else {
            trip_line
        };
        let tour = self.departures[i].tour.clone();
        let terminus = self.data.trips[self.departures[i].trip].terminus.clone();
        let Some(ci) = traffic.spawn_bus(
            world,
            renderer,
            scene,
            lead_ty.clone(),
            route,
            s,
            stops,
            number.clone(),
            hof.clone(),
            Some(scheme),
        ) else {
            return Placed::Drop;
        };
        self.car_departure.insert(traffic.cars[ci].id, i);
        if let Some(t) = &turned {
            traffic.set_trailers(world, renderer, scene, ci, &t[1..]);
            traffic.cars[ci].consist_reversed = true;
        } else if let Some(rest) = &rest {
            traffic.attach_cars(world, renderer, scene, ci, rest);
        }
        if train.is_some() {
            log::info!("train: {}", std::iter::once(traffic.cars[ci].vehicle.ty.def.path.file_stem().unwrap_or_default().to_string_lossy().to_string()).chain(traffic.cars[ci].vehicle.trailers.iter().map(|t| format!("{}{}", t.ty.def.path.file_stem().unwrap_or_default().to_string_lossy(), if t.reversed { " (turned)" } else { "" }))).collect::<Vec<_>>().join(" + "));
            traffic.cars[ci].state.max_speed_kmh = 90.0;
            traffic.cars[ci].state.length = 20.0 * (1 + traffic.cars[ci].vehicle.trailers.len()) as f32;
        }
        let names = self.trip_stop_names(self.departures[i].trip);
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let last_stop = trip_stations(&self.data.trips[self.departures[i].trip]).last().copied();
        let (always, early) = self.special_stops(i);
        let car = &mut traffic.cars[ci];
        // on its layover only when it stands at its first stop now (the trip's first station
        // may lie on a part of the track that is not loaded): it waits there for its departure
        if let Some(b) = car.bus.as_mut() {
            b.layover = departure > day_time
                && b.stops.front().map(|st| st.ri == 0 && (st.s - s).abs() < 2.0).unwrap_or(false);
            b.route_open = end < slots.len();
            b.terminus = terminus.clone();
            b.last_stop = last_stop;
            b.always = always;
            b.serve_early = early;
        }
        // the bus scripts read the line/terminus for their displays
        if let Some(i) = ty.program.str_var("Linie") {
            car.vehicle.state.str_vars[i as usize] = line.clone();
        }
        set_ai_destination(&mut car.vehicle, hof.as_deref(), &line, &terminus, &names);
        log::info!("scheduled bus: line {line} tour {tour} trip {trip_name} {} #{:?} at {:.1} min, {} stops, at ({:.1}, {:.1}) heading {:.0}{}", ty.def.type_name, number.as_ref().map(|n| format!("{} plate {:?} paint {:?}", n.0, n.1, scheme.and_then(|i| ty.paint_schemes.get(i)).map(|p| p.name.as_str()))), day_time / 60.0, car.bus.as_ref().map(|b| b.stops.len()).unwrap_or(0), car.vehicle.position.x, car.vehicle.position.y, car.vehicle.heading, if end < slots.len() { format!(", route {} of {} steps so far", end - start, steps.len()) } else { String::new() });
        if end < slots.len() {
            self.running.push(RunningTrip {
                car: car.id,
                steps,
                next: end,
                stations: stations
                    .iter()
                    .copied()
                    .zip(leave.iter().copied())
                    .collect(),
                served,
            });
        }
        if profile {
            log::info!(
                "  spawn took {:.1} ms (route {:.1} ms), {} pending, {} waiting",
                t_spawn.elapsed().as_secs_f64() * 1000.0,
                t_route.as_secs_f64() * 1000.0,
                self.pending.len(),
                self.waiting.len()
            );
        }
        Placed::Spawned
    }
}

/// Consecutive route lanes that a vehicle can drive from one into the other: linked, a lane
/// change beside it, or starting (almost) where the first ends.
fn joins(net: &Network, a: usize, b: usize) -> bool {
    net.lanes[a].next.contains(&b)
        || net.parallel(a, b)
        || (net.lanes[b].start() - net.lanes[a].end()).truncate().length() < 2.0
}

/// A station link often runs on past its station: the path search that made it went a
/// few paths beyond the stop - into a turning lane, round a corner - before the next link
/// starts back at the stop on another path (Spandau's links end so in 122 of 505 joins, the
/// extra paths mostly listed with length 0). Driven as listed, the bus turned off, then
/// jumped back and drove on the wrong side or against the traffic. Such a detour is passed
/// over (made `Absent`): where the route does not join, the lane a few steps back that
/// the next one continues from - or the lane a few steps on that continues this one - is
/// where the route really goes.
fn skip_detours(net: &Network, slots: &mut [Slot]) {
    const REACH: usize = 8;
    let lane_at = |slots: &[Slot], k: usize| match slots[k] {
        Slot::Lane(l) => Some(l),
        _ => None,
    };
    let mut i = 0;
    while i + 1 < slots.len() {
        let (Some(a), Some(b)) = (lane_at(slots, i), lane_at(slots, i + 1)) else {
            i += 1;
            continue;
        };
        if joins(net, a, b) {
            i += 1;
            continue;
        }
        // back: an earlier lane of the route that `b` continues
        let back = (i.saturating_sub(REACH)..i)
            .rev()
            .find(|&k| lane_at(slots, k).map(|x| joins(net, x, b)).unwrap_or(false));
        // on: a later lane that continues `a`
        let on = (i + 2..(i + 2 + REACH).min(slots.len()))
            .find(|&k| lane_at(slots, k).map(|x| joins(net, a, x)).unwrap_or(false));
        match (back, on) {
            (Some(k), Some(m)) if i - k <= m - i - 1 => slots[k + 1..=i].fill(Slot::Absent),
            (_, Some(m)) => slots[i + 1..m].fill(Slot::Absent),
            (Some(k), None) => slots[k + 1..=i].fill(Slot::Absent),
            (None, None) => {}
        }
        i += 1;
    }
}

/// Where consecutive lanes of a route do not join (a path the timetable file names that
/// the map does not have any more, a junction a mod map edited after its tracks were
/// made), the shortest way between them through the network, when there is one not much
/// longer than the gap: the bus drives it instead of jumping across. Returns the lanes and,
/// for each lane given, its index in them.
fn bridge_gaps(net: &Network, lanes: &[usize]) -> (Vec<usize>, Vec<usize>) {
    let mut out: Vec<usize> = Vec::with_capacity(lanes.len());
    let mut index = Vec::with_capacity(lanes.len());
    for (k, &b) in lanes.iter().enumerate() {
        if k > 0 {
            let a = lanes[k - 1];
            if !joins(net, a, b) {
                let gap = (net.lanes[b].start() - net.lanes[a].end()).truncate().length();
                if let Some(way) = way_between(net, a, b, (gap * 2.5 + 60.0) as f32) {
                    out.extend(way);
                }
            }
        }
        index.push(out.len());
        out.push(b);
    }
    (out, index)
}

/// The lanes strictly between `a` and `b` on the shortest way from the end of `a` to the
/// start of `b`, if that is at most `max` metres long.
fn way_between(net: &Network, a: usize, b: usize, max: f32) -> Option<Vec<usize>> {
    use std::cmp::Reverse;
    let mut best: HashMap<usize, (f32, usize)> = HashMap::new();
    let mut heap = std::collections::BinaryHeap::new();
    for &n in &net.lanes[a].next {
        heap.push((Reverse(ordered(0.0)), n, a));
    }
    while let Some((Reverse(c), l, from)) = heap.pop() {
        let c = c as f32 / 1000.0;
        if best.contains_key(&l) {
            continue;
        }
        best.insert(l, (c, from));
        if l == b {
            let mut way = Vec::new();
            let mut at = from;
            while at != a {
                way.push(at);
                at = best.get(&at)?.1;
            }
            way.reverse();
            return Some(way);
        }
        let c2 = c + net.lanes[l].length();
        if c2 > max {
            continue;
        }
        for &n in &net.lanes[l].next {
            if !best.contains_key(&n) {
                heap.push((Reverse(ordered(c2)), n, l));
            }
        }
    }
    None
}

/// A distance in millimetres, for ordering.
fn ordered(m: f32) -> u64 {
    (m.max(0.0) * 1000.0) as u64
}

/// A flight path: aircraft are not tied to the ground under them.
fn track_is_air(traffic: &Traffic, lane: usize) -> bool {
    traffic
        .net
        .lanes
        .get(lane)
        .map(|l| l.kind == omsi_sim::traffic::LaneKind::Air)
        .unwrap_or(false)
}

/// Where on its route a bus is: the step it is on and how far into it, from the leg it is on
/// (`leg`, `frac` of the way along) and the estimated length of every step (`est`; an absent
/// step has none, so the bus is on the next step there is). None when that is past the end.
fn step_at(
    steps: &[Step],
    slots: &[Slot],
    est: &[f64],
    leg: usize,
    frac: f64,
) -> Option<(usize, f64)> {
    let in_leg: Vec<usize> = (0..steps.len()).filter(|&k| steps[k].leg == leg).collect();
    let (mut at, mut offset) = match in_leg.last() {
        // a leg without a station link: the bus is at the start of the next one
        None => (steps.iter().position(|s| s.leg > leg)?, 0.0),
        Some(&last) => {
            let mut target = frac * in_leg.iter().map(|&k| est[k]).sum::<f64>();
            let mut pick = (last, est[last]);
            for &k in &in_leg {
                if est[k] > 0.0 && target <= est[k] {
                    pick = (k, target);
                    break;
                }
                target -= est[k];
            }
            pick
        }
    };
    while slots.get(at) == Some(&Slot::Absent) {
        at += 1;
        offset = 0.0;
    }
    (at < slots.len()).then_some((at, offset))
}

/// The steps around `at` that the network has, up to the steps still to come on either
/// side: (first, end).
fn section_around(slots: &[Slot], at: usize) -> (usize, usize) {
    let start = slots[..at]
        .iter()
        .rposition(|s| *s == Slot::Waiting)
        .map(|k| k + 1)
        .unwrap_or(0);
    let end = slots[at..]
        .iter()
        .position(|s| *s == Slot::Waiting)
        .map(|k| at + k)
        .unwrap_or(slots.len());
    (start, end)
}

/// The depot file an `[aigroup_depot]` names for a vehicle: a file of that name next to the
/// vehicle, else the one whose `[name]` it is - the stock groups name the depot
/// ("Spandau 1986"), not the file ("Spandau 86.hof"), and without it no scheduled bus had
/// termini or stops for its displays.
fn depot_file(
    cache: &mut HashMap<(std::path::PathBuf, String), Option<Arc<omsi_vehicle::Hof>>>,
    dir: &Path,
    name: &str,
) -> Option<Arc<omsi_vehicle::Hof>> {
    let key = (dir.to_path_buf(), name.trim().to_ascii_lowercase());
    if let Some(h) = cache.get(&key) {
        return h.clone();
    }
    // (through the content file system: the bus may be in an archive, and a mod may add
    // depot files to a stock bus folder)
    let mut found = omsi_vehicle::hof::depot_in(dir, name);
    if found.is_none() {
        // a mod bus brings only the depot of the map it was made on: the map's depot as
        // another vehicle folder has it (`omsi_vehicle::hof::depot_anywhere`)
        found = omsi_vehicle::hof::depot_anywhere(name);
        match &found {
            Some(h) => log::info!(
                "{} has no depot file '{name}'; using {}",
                dir.display(),
                h.path.display()
            ),
            None => log::debug!(
                "no depot file '{name}' next to {} or in any vehicle folder",
                dir.display()
            ),
        }
    }
    let h = found.map(Arc::new);
    cache.insert(key, h.clone());
    h
}

/// Put an AI bus's IBIS onto the line/terminus of its trip: the depot file gives the
/// terminus code (by ident) and the info trip (route index) for the line - the one its
/// stops (`stops`, the trip's station names) follow, [`pick_route`] - and the IBIS
/// variables the bus scripts render are set as if the driver had typed them.
pub fn set_ai_destination(
    v: &mut omsi_sim::VehicleInstance,
    hof: Option<&omsi_vehicle::Hof>,
    line: &str,
    terminus: &str,
    stops: &[&str],
) {
    set_destination(v, hof, line, terminus, stops, false)
}

/// The same for the player's bus, done the driver's way: a typing job
/// (`omsi_sim::ibis::Typist`) that works the bus's own IBIS keys - or its ticket machine's
/// - as a driver would, so that the IBIS script itself sets the displays, the stop list,
/// the announcements and the ticket printer. `stop` is the stop of the trip the bus is at.
/// None when the depot file has no such destination. The electrics must be on; when the
/// typing fails the IBIS variables are written directly ([`set_player_destination_directly`]).
/// The `ai_scheduled_settarget` trigger is only used for a hand-cranked roller blind, which
/// no IBIS drives, and only with the main switch already on: fired before the start-up it
/// switched the main switch on, and the start-up's toggle then switched the NL202's
/// electrics off again.
#[allow(clippy::too_many_arguments)]
pub fn player_ibis(
    v: &mut omsi_sim::VehicleInstance,
    hof: Option<&omsi_vehicle::Hof>,
    line: &str,
    terminus: &str,
    stops: &[&str],
    stop: Option<(usize, &str)>,
    operable: &dyn Fn(&str) -> bool,
    background: bool,
) -> Option<omsi_sim::ibis::Typist> {
    let h = hof?;
    let Some(target) = ibis_target(h, line, terminus, stops, stop) else {
        log::info!(
            "IBIS: terminus '{}' is not in depot file {}",
            terminus.trim(),
            h.name
        );
        return None;
    };
    // a roller blind is cranked by hand; the AI trigger turns it to the trip. Known by the
    // blind's own keys, not by its variables: the NL202 declares the roller blind's
    // variables without having one, and the AI trigger then gave its matrix a blank line
    // number that pushed the destination aside.
    if has_roller_blind(v)
        && v.var("elec_busbar_main_sw")
        .map(|x| x > 0.5)
        .unwrap_or(false)
    {
        set_line_to(v, line);
        v.set_var("AI_target_index", target.terminus_index as f32);
        v.trigger("ai_scheduled_settarget");
    }
    log::info!(
        "IBIS: typing line '{}' to '{}' (line {} route {:?} destination {:?}, stop {})",
        line.trim(),
        terminus.trim(),
        target.line,
        target.route,
        target.terminus_code,
        target.stop
    );
    Some(omsi_sim::ibis::Typist::new(v, target, operable, background))
}

/// The player's IBIS set without typing: the IBIS variables written as the IBIS script
/// would leave them.
pub fn set_player_destination_directly(
    v: &mut omsi_sim::VehicleInstance,
    hof: Option<&omsi_vehicle::Hof>,
    line: &str,
    terminus: &str,
    stops: &[&str],
) {
    set_destination(v, hof, line, terminus, stops, true)
}

/// What the IBIS shows once a driver has typed a trip's codes, standing at the timetable's
/// stop `stop` (index and name).
pub fn ibis_target(
    hof: &omsi_vehicle::Hof,
    line: &str,
    terminus: &str,
    stops: &[&str],
    stop: Option<(usize, &str)>,
) -> Option<omsi_sim::ibis::Target> {
    let (codes, ti) = ibis_codes(hof, line, terminus, stops)?;
    let code = hof.termini[ti].code;
    // the IBIS looks the codes up itself: the first route of the typed code, the first
    // destination of the route's code
    let terminus_index = hof
        .termini
        .iter()
        .position(|t| t.code == code)
        .unwrap_or(ti) as i32;
    let line_number = codes.line.unwrap_or(0) / 100;
    // the last two digits of the depot code are the line's letter suffix ("5E" = line 5,
    // suffix code for E), typed as its own field on the IBIS (`ls = line*100 + suffix`,
    // ibis.rs) - dropped here it left every lettered line's suffix untyped, so a bus that
    // reads it (a destination matrix testing `IBIS_Linie_Suffix` against its own reserved
    // codes, say) found 0 instead and could take the wrong branch.
    let suffix = match codes.line.unwrap_or(0) % 100 {
        0 => line_suffix_from_text(line),
        suffix => suffix,
    };
    let route_index = codes
        .route
        .and_then(|r| {
            hof.info_trips
                .iter()
                .position(|t| omsi_cfg::parse_f32(&t.code) == (line_number * 100 + r) as f32)
        })
        .map(|i| i as i32);
    // the IBIS counts the stops of its own route list, which need not be the timetable's
    // (Grundorf's timetable has two Bauernhof stations, the route one): the stop of that
    // name nearest the timetable's place in the trip - at the trip's first stop as well,
    // for a route that begins before it
    let ibis_stop = match (route_index, stop) {
        (Some(r), Some((k, name))) => ibis_stop_index(hof, r as usize, name, k).unwrap_or(0),
        _ => 0,
    };
    Some(omsi_sim::ibis::Target {
        line: line_number,
        suffix,
        route: codes.route,
        terminus_code: codes.terminus,
        route_index,
        terminus_index,
        stop: ibis_stop,
    })
}

/// A hand-cranked roller blind (SD79 or SD83 type), known by its own keys.
fn has_roller_blind(v: &omsi_sim::VehicleInstance) -> bool {
    ["rollband_sync", "rlbnd_ziel_start"]
        .iter()
        .any(|t| v.ty.program.trigger(t).is_some())
}

/// `SetLineTo` for the AI trigger. A roller blind turns one roller per character -
/// hundreds, tens, units, with the letter suffixes only on the later rollers - so its
/// line is right-aligned to three places ("  5", " 5E"); the matrix scripts take the line
/// as it is.
fn set_line_to(v: &mut omsi_sim::VehicleInstance, line: &str) {
    let digits: String = line.trim().chars().take_while(|c| c.is_ascii_digit()).collect();
    let text = if has_roller_blind(v) {
        format!("{:>3}", line.trim())
    } else if v.ty.program.str_var("Matrix_Nmr").is_some() && !digits.is_empty() && digits.len() <= 3 {
        // the LiAZ's 4-character matrix shows three digits and a letter (see
        // omsi_script::compat): its AI path takes SetLineTo as it is
        format!("{:0>3}{}", digits, &line.trim()[digits.len()..])
    } else {
        line.trim().to_string()
    };
    if let Some(i) = v.ty.program.str_var("SetLineTo") {
        v.state.str_vars[i as usize] = text;
    }
}

fn complex_line_text(line: &str, line_num: f32) -> String {
    let line = line.trim();
    if line.chars().all(|c| c.is_ascii_digit()) {
        format!("{:03}  ", line_num as i32)
    } else {
        format!("{line:>5}")
    }
}

/// The letter and digits of a line named letter first ("X10", "M41"), else None.
fn line_prefix(line: &str) -> Option<(char, &str)> {
    let line = line.trim();
    let first = line.chars().next().filter(|c| c.is_ascii_alphabetic())?;
    let digits = &line[1..];
    (!digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
        .then_some((first.to_ascii_uppercase(), digits))
}

fn line_suffix_from_text(line: &str) -> u32 {
    // the stock MAN matrices' and X10 Berlin's IBIS's "letter then number" codes
    if let Some((letter, _)) = line_prefix(line) {
        return match letter {
            'E' => 1,
            'S' => 5,
            'A' => 6,
            'D' => 11,
            'C' => 12,
            'B' => 13,
            'U' => 25,
            'M' => 28,
            'N' => 35,
            'X' => 36,
            _ => 0,
        };
    }
    match line.trim().chars().last().map(|c| c.to_ascii_uppercase()) {
        // The stock Matrix scripts use two different E codes: 1 renders E5,
        // while 10 renders 5E. Timetable line names put the letter after the
        // number, so use the latter representation here.
        Some('E') => 10,
        // These are the corresponding "number then letter" branches in the
        // stock MAN matrix scripts. Codes 1/2/3 are not generic suffixes:
        // they render prefixes/special test text and must not be guessed from
        // the letter's alphabetic position.
        Some('U') => 31,
        Some('N') => 4,
        Some('S') => 23,
        Some('M') => 32,
        _ => 0,
    }
}

/// The numeric part of a HOF line code is the line; its route suffix is a
/// separate route selector, not automatically a display-letter code. For a
/// timetable line such as `5E`, the display suffix must therefore come from
/// the text (`10` in the stock matrix scripts), while a plain `5` stays `500`.
fn line_code_from_text(line: &str, route_code: Option<u32>) -> Option<u32> {
    // a lettered line's IBIS number is the depot file's (X10 Berlin types X10 as 510)
    if let (Some(_), Some(code)) = (line_prefix(line), route_code) {
        return Some(code / 100 * 100 + line_suffix_from_text(line));
    }
    // (four and five digit lines too: the IBIS takes line x 100 + suffix whatever the
    // line's length, and a São Paulo 7110 fell back to its route code, whose last two
    // digits - the route, not a suffix - came out on the display as a letter, #459)
    match line_number_digits(line)
        .parse::<u32>()
        .ok()
        .filter(|n| *n > 0 && *n < 100_000)
    {
        Some(number) => Some(number * 100 + line_suffix_from_text(line)),
        None => route_code,
    }
}

/// The line's number: its leading digits, or the digits after a prefix letter ("X10" → 10).
fn line_number_digits(line: &str) -> String {
    match line_prefix(line) {
        Some((_, digits)) => digits.to_string(),
        None => line
            .trim()
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect(),
    }
}

/// The key presses a driver makes on the IBIS for a trip.
#[derive(Debug, Clone, PartialEq)]
pub struct IbisCodes {
    /// Complete depot line code: numeric line × 100 plus the letter suffix
    /// (the IBIS reads the last two digits as a suffix; 0 = none).
    pub line: Option<u32>,
    /// Route entry (the last two digits of the depot file's route code).
    pub route: Option<u32>,
    /// Destination entry, when there is no route to type.
    pub terminus: Option<u32>,
}

/// A timetable's terminus is a stop name, while a HOF often uses a shorter
/// destination/texture name for the same stop (Berlin 5E: `Spektefeld
/// Schulzentrum` vs. `Spektefeld`).  Exact matches remain preferred; the
/// boundary-aware prefix fallback handles those stock abbreviations without
/// making unrelated destinations match.
fn terminus_match_score(t: &omsi_vehicle::hof::Terminus, wanted: &str) -> u8 {
    let wanted = wanted.split_whitespace().collect::<Vec<_>>().join(" ");
    if wanted.is_empty() {
        return 0;
    }
    let mut score = 0;
    for candidate in std::iter::once(t.texture_id.as_str())
        .chain(std::iter::once(t.terminus_stop.as_deref().unwrap_or("")))
        .chain(t.strings.iter().map(String::as_str))
    {
        let candidate = candidate.split_whitespace().collect::<Vec<_>>().join(" ");
        let candidate_lower = candidate.to_lowercase();
        let wanted_lower = wanted.to_lowercase();
        if candidate_lower == wanted_lower {
            score = score.max(2);
        } else if wanted_lower.starts_with(&(candidate_lower.clone() + " ")) {
            score = score.max(1);
        } else if candidate_lower.starts_with(&(wanted_lower + " ")) {
            score = score.max(1);
        }
    }
    score
}

/// The depot file's terminus a trip's destination names. OMSI takes the first whose ident
/// is the name (Omsi.exe TRoadVehicleInst.virtual_10: that row is `AI_target_index`); else
/// the best of the looser matches - the first of equals, not the last (a depot file whose
/// codes are not in row order put the AI bus's matrix on another terminus's picture, #110).
fn find_terminus(hof: &omsi_vehicle::Hof, wanted: &str) -> Option<usize> {
    let exact = wanted.trim();
    if let Some(i) = hof.termini.iter().position(|t| t.texture_id == exact) {
        return Some(i);
    }
    let mut best: Option<(usize, u8)> = None;
    for (i, t) in hof.termini.iter().enumerate() {
        let score = terminus_match_score(t, wanted);
        if score > 0 && best.is_none_or(|(_, b)| score > b) {
            best = Some((i, score));
        }
    }
    best.map(|(i, _)| i)
}

/// The IBIS codes of a trip from the depot file, and the terminus index they lead to.
/// A line has one route per direction and variant to the same terminus; the one whose
/// stop list follows the trip's stops (`stops`, the timetable's names) best is taken
/// ([`pick_route`]), else the first.
pub fn ibis_codes(
    hof: &omsi_vehicle::Hof,
    line: &str,
    terminus: &str,
    stops: &[&str],
) -> Option<(IbisCodes, usize)> {
    let terminus = terminus.trim();
    if terminus.is_empty() {
        return None;
    }
    let ti = find_terminus(hof, terminus)?;
    let code = hof.termini[ti].code;
    let route = pick_route(hof, &routes_to(hof, line, code), stops)
        .and_then(|i| hof.info_trips[i].code.trim().parse::<u32>().ok());
    let codes = match route {
        Some(r) => IbisCodes {
            line: line_code_from_text(line, Some(r)),
            route: Some(r % 100),
            terminus: None,
        },
        None => IbisCodes {
            line: line_code_from_text(line, None),
            route: None,
            terminus: u32::try_from(code).ok().filter(|c| *c < 1000),
        },
    };
    Some((codes, ti))
}

/// The depot file's routes of `line` to the terminus with code `code`, in file order. The
/// route code is the line's number and two digits: a driver types those, whatever the
/// route's line string says (Grundorf's 7601 to Krankenhaus has "TML").
fn routes_to(hof: &omsi_vehicle::Hof, line: &str, code: i32) -> Vec<usize> {
    let line_digits: String = line
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let line_number = line_digits.parse::<u32>().ok();
    hof.info_trips
        .iter()
        .enumerate()
        .filter(|(_, t)| {
            omsi_cfg::parse_i32(&t.route) == code
                && (t.line.trim().eq_ignore_ascii_case(line.trim())
                || (!line_digits.is_empty() && t.line.trim() == line_digits)
                || (line_number.is_some()
                && t.code.trim().parse::<u32>().ok().map(|c| c / 100) == line_number))
        })
        .map(|(i, _)| i)
        .collect()
}

/// Which of `routes` a trip through `stops` (the timetable's station names) runs: one
/// starting at the trip's first stop (the IBIS starts its count there), of those the one
/// that has most of the trip's stops in the trip's order, then the one as long as the trip
/// (a short working's route rather than the long one it is part of). Nothing in the
/// timetable ties a trip to a route of the depot file - a driver picks it by its stops -
/// and a line often has several routes to one terminus; taking the first whose first stop
/// was spelt as the timetable spells it typed the wrong one whenever the spellings
/// differed. None without routes; the first route when no stop matches any.
fn pick_route(hof: &omsi_vehicle::Hof, routes: &[usize], stops: &[&str]) -> Option<usize> {
    let trip: Vec<(String, Vec<String>)> = stops
        .iter()
        .map(|s| (s.trim().to_lowercase(), stop_words(s)))
        .filter(|(s, _)| !s.is_empty())
        .collect();
    let mut best: Option<(usize, (bool, usize, std::cmp::Reverse<usize>))> = None;
    for &r in routes {
        let list = hof
            .info_busstop_lists
            .get(r)
            .map(|l| l.as_slice())
            .unwrap_or(&[]);
        let names: Vec<Vec<(String, Vec<String>)>> =
            list.iter().map(|id| ident_names(hof, id)).collect();
        // the longest common subsequence: a stop missing on either side costs nothing
        // but itself, and a stop the route lists twice is not
        // matched past the rest of the trip
        let mut row = vec![0usize; names.len() + 1];
        for t in &trip {
            let mut diag = 0;
            for (j, n) in names.iter().enumerate() {
                let up = row[j + 1];
                row[j + 1] = if same_stop(n, t) { diag + 1 } else { up.max(row[j]) };
                diag = up;
            }
        }
        let found = row[names.len()];
        let starts = match (trip.first(), names.first()) {
            (Some(t), Some(n)) => same_stop(n, t),
            _ => false,
        };
        let score = (
            starts,
            found,
            std::cmp::Reverse(names.len().abs_diff(stops.len())),
        );
        if best.as_ref().is_none_or(|(_, b)| score > *b) {
            best = Some((r, score));
        }
    }
    match best {
        Some((r, (_, found, _))) if found > 0 => Some(r),
        _ => routes.first().copied(),
    }
}

/// The names a stop of a route's list goes by: its ident (before a `#`) and the strings
/// the depot file's `[addbusstop]` of that ident gives it, each lowercased and as its
/// [`stop_words`].
fn ident_names(hof: &omsi_vehicle::Hof, ident: &str) -> Vec<(String, Vec<String>)> {
    let ident = ident.split('#').next().unwrap_or("").trim();
    let mut names = vec![ident.to_string()];
    for b in &hof.bus_stops {
        if b.ident.trim().eq_ignore_ascii_case(ident) {
            names.extend(b.strings.iter().map(|s| s.trim().to_string()));
        }
    }
    names
        .into_iter()
        .filter(|n| !n.is_empty())
        .map(|n| (n.to_lowercase(), stop_words(&n)))
        .collect()
}

/// One stop of a route (its [`ident_names`]) and a timetable stop (lowercased, and its
/// [`stop_words`]) are the same: a name equal, one the start of the other, or the same
/// words.
fn same_stop(names: &[(String, Vec<String>)], stop: &(String, Vec<String>)) -> bool {
    names.iter().any(|(raw, words)| {
        !raw.is_empty()
            && (*raw == stop.0
            || raw.starts_with(&stop.0)
            || stop.0.starts_with(raw.as_str())
            || (!words.is_empty() && *words == stop.1))
    })
}

/// A stop name as the set of its words, so that the map's and the depot file's spellings
/// of one stop meet: in any order ("Nordstadt Bhf", "Bhf Nordstadt"), with any punctuation
/// ("Bhf. Nordstadt") and without one-letter prefixes ("F_Kirchweg", "Kirchweg").
fn stop_words(name: &str) -> Vec<String> {
    let mut w: Vec<String> = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() > 1)
        .map(|w| w.to_lowercase())
        .collect();
    w.sort();
    w
}

fn set_destination(
    v: &mut omsi_sim::VehicleInstance,
    hof: Option<&omsi_vehicle::Hof>,
    line: &str,
    terminus: &str,
    stops: &[&str],
    player: bool,
) {
    let Some(hof) = hof else { return };
    let terminus = terminus.trim();
    if terminus.is_empty() {
        return;
    }
    let term_index = find_terminus(hof, terminus);
    let Some(ti) = term_index else {
        log::debug!(
            "AI bus: terminus '{terminus}' not in depot file {} ({} termini)",
            hof.name,
            hof.termini.len()
        );
        return;
    };
    log::debug!(
        "AI bus: line {line} terminus '{terminus}' → depot terminus {ti} code {}",
        hof.termini[ti].code
    );
    let code = hof.termini[ti].code;
    let route_index = pick_route(hof, &routes_to(hof, line, code), stops);
    let line_num = line_number_digits(line).parse::<f32>().unwrap_or(0.0);
    // The route's last two digits select its stop list; they must not replace
    // a display suffix. Otherwise an ordinary route code such as 505 becomes
    // suffix 5 and the stock matrix renders S5 instead of 5E.
    let route_code = route_index
        .and_then(|i| hof.info_trips.get(i))
        .and_then(|t| t.code.trim().parse::<u32>().ok());
    let line_code =
        line_code_from_text(line, route_code).unwrap_or_else(|| line_num.max(0.0) as u32 * 100);
    let line_suffix = (line_code % 100) as f32;
    let line_num = if line_prefix(line).is_some() {
        (line_code / 100) as f32
    } else {
        line_num
    };
    // the original's way: SetLineTo + AI_target_index, then the ai_scheduled_settarget trigger
    set_line_to(v, line);
    if !player {
        v.set_var("AI_target_index", ti as f32);
        if v.trigger("ai_scheduled_settarget") {
            v.set_var(
                "IBIS_RouteIndex",
                route_index.map(|r| r as f32).unwrap_or(-1.0),
            );
            return;
        }
    }
    v.set_var("IBIS_LinieKurs", line_num);
    v.set_var("IBIS_Linie_Complex", line_code as f32);
    v.set_var("IBIS_Linie_Suffix", line_suffix);
    v.set_var("IBIS_TerminusIndex", ti as f32);
    v.set_var("IBIS_TerminusCode", code as f32);
    v.set_var(
        "IBIS_RouteIndex",
        route_index.map(|r| r as f32).unwrap_or(-1.0),
    );
    v.set_var("IBIS_mode", 0.0);
    let set_str = |v: &mut omsi_sim::VehicleInstance, name: &str, val: String| {
        if let Some(i) = v.ty.program.str_var(name) {
            v.state.str_vars[i as usize] = val;
        }
    };
    set_str(
        v,
        "IBIS_terminus_name",
        hof.termini[ti].strings.first().cloned().unwrap_or_default(),
    );
    let complex = if line_num > 0.0 {
        complex_line_text(line, line_num)
    } else {
        "     ".into()
    };
    set_str(v, "IBIS_Complex_Line", complex);
    // terminus texture change ident for roller blinds / matrix textures
    set_str(
        v,
        "IBIS_terminus_texture",
        hof.termini[ti].texture_id.clone(),
    );
}

/// One stop of a planned trip with its scheduled times (seconds since midnight).
#[derive(Debug, Clone)]
pub struct PlannedStop {
    pub object_id: i64,
    pub name: String,
    pub arr: f64,
    pub dep: f64,
    pub position: Option<glam::DVec3>,
    /// Which way the trip runs through the stop ([`StopDir`]): a circular route, or one
    /// that turns back, calls at the same place twice and the two stops of it stand a few
    /// metres apart. Only the direction says which of them a bus has reached (#254).
    pub dir: StopDir,
    /// The bus stops here (a depot run passes its stations).
    pub stops: bool,
}

/// Which way a trip runs through one of its stops: the direction it arrives on and the one
/// it leaves on, as unit vectors of the ground plane (x east, y north). None where a
/// neighbour's place is unknown or too near to tell a direction - any heading will do then.
#[derive(Debug, Clone, Copy, Default)]
pub struct StopDir {
    pub inbound: Option<glam::DVec2>,
    pub outbound: Option<glam::DVec2>,
}

impl StopDir {
    fn takes(self, fwd: glam::DVec2) -> bool {
        if self.inbound.is_none() && self.outbound.is_none() {
            return true;
        }
        [self.inbound, self.outbound].into_iter().flatten().any(|d| fwd.dot(d) >= DIR_COS)
    }
}

/// The unit vector of the ground plane a bus heading `deg` drives along (degrees clockwise
/// from north, as `VehicleInstance::heading`).
fn forward_of(deg: f64) -> glam::DVec2 {
    let h = deg.to_radians();
    glam::DVec2::new(h.sin(), h.cos())
}

/// How far apart two stops of a trip must stand before the line between them is taken as
/// the way the trip runs between them (m).
const DIR_REACH: f64 = 20.0;
/// How far off the way a trip runs through a stop a bus may head and still be taken as
/// running that way: the cosine of the angle, 60 degrees either side.
const DIR_COS: f64 = 0.5;

#[derive(Debug, Clone)]
pub struct PlannedTrip {
    pub name: String,
    pub line: String,
    pub terminus: String,
    pub departure: f64,
    /// Arrival at the last station.
    pub end: f64,
    pub stops: Vec<PlannedStop>,
}

impl PlannedTrip {
    /// Give every stop the way the trip runs through it: in from the stop before, out to
    /// the stop after.
    fn set_dirs(&mut self) {
        let p: Vec<Option<glam::DVec3>> = self.stops.iter().map(|s| s.position).collect();
        let dir = |a: Option<glam::DVec3>, b: Option<glam::DVec3>| -> Option<glam::DVec2> {
            let v = (b? - a?).truncate();
            (v.length() >= DIR_REACH).then(|| v.normalize())
        };
        for (i, s) in self.stops.iter_mut().enumerate() {
            s.dir = StopDir {
                inbound: i.checked_sub(1).and_then(|k| dir(p[k], p[i])),
                outbound: p.get(i + 1).and_then(|b| dir(p[i], *b)),
            };
        }
    }
}

/// The bus is at a stop within this distance (m), and has left it beyond the second.
const AT_STOP: f64 = 25.0;
const LEFT_STOP: f64 = 35.0;

/// How far ahead (s) the departure displays look.
const BOARD_AHEAD: f64 = 2.0 * 3600.0;
/// Most departures a page gets for a stop (`omsi.getDepartures`).
const MAX_PAGE_DEPARTURES: usize = 20;

/// Where timetable stop `k` (called `name`) stands in the IBIS's own stop list of route
/// `route` (an index into the depot file's `info_busstop_lists`): the stop of that name
/// nearest `k`. What `IBIS_busstop` has to be for the IBIS to show that stop.
pub fn ibis_stop_index(hof: &omsi_vehicle::Hof, route: usize, name: &str, k: usize) -> Option<usize> {
    let stop = (name.trim().to_lowercase(), stop_words(name));
    let list = hof.info_busstop_lists.get(route)?;
    // (spelt as `pick_route` compares the names: "Kirchweg" is the depot file's "F_Kirchweg")
    list.iter()
        .enumerate()
        .filter(|(_, id)| same_stop(&ident_names(hof, id), &stop))
        .map(|(i, _)| i)
        .min_by_key(|i| i.abs_diff(k))
}

/// The player's tour: its trips with planned stop times, and the progress along them.
pub struct PlayerDuty {
    pub line: String,
    pub tour: String,
    pub trips: Vec<PlannedTrip>,
    pub trip_index: usize,
    /// Where `trips` begins in the tour: a picked trip is a duty of its own, and a saved
    /// situation counts the trip under way from the tour's first.
    pub first_trip: usize,
    /// Next stop to serve on the current trip.
    pub next_stop: usize,
    /// True while the bus stands at the next stop.
    at_stop: bool,
    /// How late the bus arrived at the stop it stands at (s after its arrival time).
    arrived_late: Option<f64>,
    /// The bus has reached the last stop of the current trip.
    done: bool,
    /// The last stop where this trip actually stopped with a passenger door open.
    served_terminus: Option<glam::DVec3>,
    /// How late (s, negative = early) the bus left the last stop it served on this trip;
    /// None while it has not left one.
    left_late: Option<f64>,
    /// A page moved the duty back to an earlier stop: `catch_up` must not jump forward
    /// again to a later stop the bus still stands at, until the bus reaches a stop again.
    held_back: bool,
    /// The first update looks where the bus stands.
    placed: bool,
    /// The current trip changed since the last `take_trip_change`.
    trip_changed: bool,
    /// The player picked the current trip: the duty does not move on past it before it is
    /// driven (or given up), however late the bus is for it.
    picked: bool,
    /// Time of day of the first update (placing waits a little for the places of stops
    /// beyond the loaded tiles, see `learn_places`).
    first_update: Option<f64>,
    /// The way the bus faces (degrees clockwise from north), from the last update: it says
    /// which of two stops a few metres apart the bus is at (see `StopDir`).
    heading: f64,
}

impl Schedule {
    /// The stops of a tour in the order it drives them, over all its trips: (trip number in
    /// the duty, stop number in the trip, name, departure there in seconds). Passing stations
    /// are left out.
    pub fn tour_stops(&self, line: &str, tour: &str) -> Vec<(usize, usize, String, f64)> {
        let Some(l) = self.data.lines.iter().find(|l| l.name.eq_ignore_ascii_case(line)) else { return Vec::new() };
        let Some(t) = l.tours.iter().find(|t| t.number.eq_ignore_ascii_case(tour)) else { return Vec::new() };
        let mut out = Vec::new();
        let mut k = 0;
        for tt in &t.trips {
            let Some(ti) = self.data.trips.iter().position(|x| x.name.eq_ignore_ascii_case(&tt.trip)) else { continue };
            let trip = &self.data.trips[ti];
            let departure = tt.departure as f64 * 60.0;
            let times = &self.times[ti][usize::try_from(tt.profile).unwrap_or(0).min(self.times[ti].len() - 1)];
            for (i, id) in trip_stations(trip).iter().enumerate() {
                if !times.stops.get(i).copied().unwrap_or(true) {
                    continue;
                }
                let name = self
                    .data
                    .bus_stops
                    .iter()
                    .find(|b| b.object_id == *id)
                    .map(|b| b.name.clone())
                    .filter(|n| !n.trim().is_empty())
                    .or_else(|| trip.stations.is_empty().then(|| trip.stations_legacy.get(i).and_then(|r| r.get(2)).map(|n| n.trim().to_string())).flatten())
                    .unwrap_or_else(|| format!("{}", i + 1));
                out.push((k, i, name, departure + times.stations[i].1));
            }
            k += 1;
        }
        out
    }

    /// How many trips a tour has that the timetable knows (the trips `tour_stops` numbers).
    pub fn tour_trip_count(&self, line: &str, tour: &str) -> usize {
        let mut n = 0;
        let mut last = None;
        for s in self.tour_stops(line, tour) {
            if last != Some(s.0) {
                n += 1;
                last = Some(s.0);
            }
        }
        n
    }

    /// The position (in order of departure, as `tour_trip_stops` counts) of the trip of a
    /// tour that is under way or next to leave at `now` (seconds of the day).
    pub fn tour_trip_now(&self, line: &str, tour: &str, now: f64) -> usize {
        let all = self.tour_stops(line, tour);
        let mut order: Vec<(usize, f64, f64)> = Vec::new();
        for s in &all {
            match order.iter_mut().find(|o| o.0 == s.0) {
                Some(o) => o.2 = o.2.max(s.3),
                None => order.push((s.0, s.3, s.3)),
            }
        }
        order.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
        order.iter().position(|o| o.2 >= now - 60.0).unwrap_or(0)
    }

    /// All the stops of the trip in position `pos` of the tour's trips in order of the time
    /// they leave. The trip number in the entries is the one `tour_stops` gives it.
    pub fn tour_trip_stops(&self, line: &str, tour: &str, pos: usize) -> Vec<(usize, usize, String, f64)> {
        let all = self.tour_stops(line, tour);
        let mut order: Vec<(usize, f64)> = Vec::new();
        for s in &all {
            if !order.iter().any(|o| o.0 == s.0) {
                order.push((s.0, s.3));
            }
        }
        order.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
        let Some(&(k, _)) = order.get(pos) else { return Vec::new() };
        all.into_iter().filter(|s| s.0 == k).collect()
    }

    /// The stops of the trip of a tour that is under way or next to start at `now` (seconds
    /// of the day): the first trip with a stop still to come (a minute of grace), and only
    /// that trip's stops from there on. A tour with nothing left today lists its first trip
    /// whole. Same entries and numbering as `tour_stops`.
    pub fn tour_stops_from(&self, line: &str, tour: &str, now: f64) -> Vec<(usize, usize, String, f64)> {
        let all = self.tour_stops(line, tour);
        let limit = now - 60.0;
        let Some(k) = all.iter().find(|s| s.3 >= limit).map(|s| s.0).or_else(|| all.first().map(|s| s.0)) else { return Vec::new() };
        let trip: Vec<_> = all.into_iter().filter(|s| s.0 == k).collect();
        let rest: Vec<_> = trip.iter().filter(|s| s.3 >= limit).cloned().collect();
        if rest.is_empty() { trip } else { rest }
    }

    /// The player drives this tour (line and tour as `player_duty` names them): OMSI leaves
    /// it to the player, so no AI bus runs it as well - one of line 76 tour 1 appeared
    /// 5 m beside the player's own bus at the Bauernhof, where the passengers queued.
    /// Returns how many of today's departures that takes from the AI.
    pub fn reserve_tour(&mut self, line: &str, tour: &str) -> usize {
        self.player_tour = Some((line.to_string(), tour.to_string()));
        let mine: Vec<bool> = (0..self.departures.len())
            .map(|i| self.is_player_tour(i))
            .collect();
        let mut n = 0;
        for (d, m) in self.departures.iter_mut().zip(&mine) {
            if *m {
                n += 1;
                d.spawned = true;
            }
        }
        self.pending.retain(|i| !mine[*i]);
        self.waiting.retain(|i| !mine[*i]);
        self.retry_at.retain(|i, _| !mine[*i]);
        self.purge_player_tour = true;
        log::info!("timetable: {n} departures of line {line} tour {tour} left to the player");
        n
    }

    /// LAN play (host): the tours the other players drive now. A tour taken leaves the
    /// timetable like the player's own (its bus on the road goes); one given up (the player
    /// left, or took another duty) runs again from its next departure.
    pub fn set_lan_tours(&mut self, tours: HashSet<(String, String)>) {
        if tours == self.lan_tours {
            return;
        }
        let added: Vec<(String, String)> = tours.difference(&self.lan_tours).cloned().collect();
        let removed: Vec<(String, String)> = self.lan_tours.difference(&tours).cloned().collect();
        self.lan_tours = tours;
        let of = |d: &Departure, t: &(String, String)| d.line.eq_ignore_ascii_case(&t.0) && d.tour.eq_ignore_ascii_case(&t.1);
        for t in &removed {
            let later: Vec<usize> = (0..self.departures.len())
                .filter(|&i| of(&self.departures[i], t) && self.departures[i].time > self.last_tod && !self.is_player_tour(i))
                .collect();
            for &i in &later {
                self.departures[i].spawned = false;
            }
            log::info!("timetable: line {} tour {} is no LAN player's any more: its {} later departures run again", t.0, t.1, later.len());
        }
        if !added.is_empty() {
            let mine: Vec<bool> = (0..self.departures.len()).map(|i| self.is_player_tour(i)).collect();
            for (d, m) in self.departures.iter_mut().zip(&mine) {
                if *m {
                    d.spawned = true;
                }
            }
            self.pending.retain(|i| !mine[*i]);
            self.waiting.retain(|i| !mine[*i]);
            self.retry_at.retain(|i, _| !mine[*i]);
            self.purge_player_tour = true;
            for t in &added {
                log::info!("timetable: line {} tour {} left to a LAN player", t.0, t.1);
            }
        }
    }

    /// Does a player drive departure `i` (we, or another LAN player)?
    fn is_player_tour(&self, i: usize) -> bool {
        if let Some(d) = self.departures.get(i) {
            if self.lan_tours.iter().any(|t| d.line.eq_ignore_ascii_case(&t.0) && d.tour.eq_ignore_ascii_case(&t.1)) {
                return true;
            }
        }
        match (&self.player_tour, self.departures.get(i)) {
            (Some((line, tour)), Some(d)) => {
                d.line.eq_ignore_ascii_case(line)
                    && d.tour.eq_ignore_ascii_case(tour)
                    && self.player_departure.map(|t| (d.time - t).abs() < 30.0).unwrap_or(true)
            }
            _ => false,
        }
    }

    /// Assign the player a tour of a line (names as in the .ttl / `[newtour]`), starting
    /// with the trip that fits the time of day `now`: the one under way, else the next to
    /// leave - as OMSI does when a time is picked for a tour. (The duty used to start with
    /// the tour's first trip whatever the time: Spandau's "Mo-Fr 3" of line 5 at 15:05 began
    /// with the 14:44 depot run, and the IBIS was typed for it.) The AI no longer drives the
    /// tour: its trips are the player's ([`Schedule::reserve_tour`]).
    /// When the line or its tour cannot be driven, the error says why in words a driver can
    /// act on (a line the date's chrono takes off names the chrono and the day it starts);
    /// the callers used to drop that silently and the game started without a duty.
    ///
    /// `trip` is the trip to start with as the player picked it (OMSI's timetable dialog
    /// has line, tour and trip): its departure time "HH:MM" - the first trip leaving then or
    /// later - or its number in the tour (1 = first). The duty goes on with the tour's next
    /// trips from there, as OMSI's does.
    pub fn player_duty(
        &mut self,
        world: &World,
        line: &str,
        tour: &str,
        now: f64,
        trip: Option<&str>,
        whole_tour: bool,
    ) -> Result<PlayerDuty, String> {
        let date = |code: i32| {
            format!(
                "{:04}-{:02}-{:02}",
                code / 10000,
                code / 100 % 100,
                code % 100
            )
        };
        let Some(l) = self
            .data
            .lines
            .iter()
            .find(|l| l.name.eq_ignore_ascii_case(line))
        else {
            let running = self.data.lines.len();
            if let Some((name, dir)) = self
                .deactivated
                .iter()
                .find(|(l, _)| l.trim().eq_ignore_ascii_case(line.trim()))
            {
                let from = Some(omsi_cfg::resolve_path(dir, "Chrono.cfg"))
                    .and_then(|p| omsi_cfg::CfgFile::read(&p).ok())
                    .map(|f| omsi_map::ailists::parse_chrono_cfg(&f).start_date)
                    .filter(|d| *d > 0);
                return Err(format!(
                    "line {name} does not run on {}: the timetable change '{}'{} takes it off. {running} other lines run that day: pick one of them, or an earlier date",
                    date(world.date),
                    dir.file_name().unwrap_or_default().to_string_lossy(),
                    from.map(|d| format!(" of {}", date(d))).unwrap_or_default()
                ));
            }
            return Err(format!("line {line} is not in the timetable of this map on {}: {running} lines run that day", date(world.date)));
        };
        let t = match l.tours.iter().find(|t| t.number.eq_ignore_ascii_case(tour)) {
            Some(t) => t,
            None => {
                let Some(first) = l.tours.first() else {
                    return Err(format!("line {} has no tours", l.name));
                };
                if !tour.is_empty() {
                    log::warn!(
                        "line {} has no tour '{tour}': driving its tour {}",
                        l.name,
                        first.number
                    );
                }
                first
            }
        };
        let mut trips = Vec::new();
        for tt in &t.trips {
            let Some(ti) = self
                .data
                .trips
                .iter()
                .position(|x| x.name.eq_ignore_ascii_case(&tt.trip))
            else {
                continue;
            };
            let trip = &self.data.trips[ti];
            let departure = tt.departure as f64 * 60.0;
            let times = &self.times[ti][usize::try_from(tt.profile)
                .unwrap_or(0)
                .min(self.times[ti].len() - 1)];
            let stops = trip_stations(trip)
                .iter()
                .enumerate()
                .map(|(i, id)| {
                    let name = self.station_name(trip, i, *id);
                    let position = world.object_positions.lock().get(id).map(|p| p.0);
                    let (arr, dep) = times.stations[i];
                    PlannedStop {
                        object_id: *id,
                        name,
                        arr: departure + arr,
                        dep: departure + dep,
                        position,
                        dir: StopDir::default(),
                        stops: times.stops[i],
                    }
                })
                .collect();
            let mut planned = PlannedTrip {
                name: trip.name.clone(),
                line: trip.line.clone(),
                terminus: trip.terminus.clone(),
                departure,
                end: departure + times.duration,
                stops,
            };
            planned.set_dirs();
            trips.push(planned);
        }
        if trips.is_empty() {
            return Err(format!(
                "line {} tour {} has no trip the timetable knows ({} listed)",
                l.name,
                t.number,
                t.trips.len()
            ));
        }
        let (line_name, tour_name) = (l.name.clone(), t.number.clone());
        let trip_index = match trip.map(str::trim).filter(|t| !t.is_empty()) {
            Some(pick) => chosen_trip(&trips, pick).ok_or_else(|| {
                format!(
                    "line {line_name} tour {tour_name} has no trip '{pick}': its {} trips leave {}",
                    trips.len(),
                    trips.iter().map(|t| hhmm(t.departure)).collect::<Vec<_>>().join(", ")
                )
            })?,
            None => starting_trip(&trips, now),
        };
        let hm = |t: f64| {
            format!(
                "{:02}:{:02}",
                (t / 3600.0) as i32,
                ((t % 3600.0) / 60.0) as i32
            )
        };
        // A picked trip is the duty, one way to its terminus, as a trip chosen in OMSI is;
        // the rest of the tour only with `--whole-tour`.
        let (trips, trip_index, first_trip) = if trip.is_some() && !whole_tour {
            (vec![trips[trip_index].clone()], 0, trip_index)
        } else {
            (trips, trip_index, 0)
        };
        // the AI leaves the player what the player drives: the tour, or just the one trip
        self.player_departure = (trips.len() == 1 && trip.is_some() && !whole_tour).then(|| trips[0].departure);
        let reserved = self.reserve_tour(&line_name, &tour_name);
        let current = &trips[trip_index];
        log::info!(
            "player duty: line {line_name} tour {tour_name}: {} trips from {} ({} of them not driven by the AI); at {} trip {} {} {} (to {}), then {}",
            trips.len(),
            hm(trips[0].departure),
            reserved,
            hm(now),
            trip_index + 1,
            current.name,
            // (a trip after midnight picked in the evening leaves tonight)
            if current.departure < now && now - current.departure < DAY / 2.0 { format!("under way since {}", hm(current.departure)) } else { format!("leaves at {}", hm(current.departure)) },
            current.terminus,
            trips.get(trip_index + 1).map(|t| format!("{} at {}", t.name, hm(t.departure))).unwrap_or_else(|| "the end of the duty".into())
        );
        Ok(PlayerDuty {
            line: line_name,
            tour: tour_name,
            trips,
            trip_index,
            first_trip,
            next_stop: 0,
            at_stop: false,
            arrived_late: None,
            done: false,
            served_terminus: None,
            left_late: None,
            held_back: false,
            placed: false,
            trip_changed: false,
            picked: trip.map(|t| !t.trim().is_empty()).unwrap_or(false),
            first_update: None,
            heading: 0.0,
        })
    }

    /// The buses due at one bus stop (map object id) within the next two hours, unsorted, as
    /// (expected arrival, line, terminus, time it stands at the stop), all in seconds of the day:
    /// the timetable buses with the delay they run with, and the player's.
    fn stop_list(
        &self,
        stop: i64,
        now: f64,
        on_road: &HashMap<usize, OnRoad>,
        duty: Option<&PlayerDuty>,
        player_hof: Option<&omsi_vehicle::Hof>,
    ) -> Vec<(f64, String, String, f64)> {
        let mut list: Vec<(f64, String, String, f64)> = Vec::new();
        for &(trip, k) in self.visits.get(&stop).map(|v| v.as_slice()).unwrap_or(&[]) {
            for &i in &self.trip_departures[trip] {
                if !self.runs(i) {
                    continue;
                }
                let d = &self.departures[i];
                let tt = &self.times[d.trip][d.profile];
                if !tt.stops[k] {
                    continue;
                }
                let (arrive, leave) = (d.time + tt.stations[k].0, d.time + tt.stations[k].1);
                if arrive > now + BOARD_AHEAD || leave < now - BOARD_AHEAD {
                    continue;
                }
                if self.is_player_tour(i) {
                    continue;
                }
                let expected = match on_road.get(&i) {
                    Some(r) => match r.next {
                        // the stations before the bus's next stop are behind it
                        Some(next) if leave < next - 0.5 => continue,
                        None => continue,
                        // standing at this stop
                        Some(next) if r.dwelling && (leave - next).abs() < 0.5 => now,
                        Some(next) => {
                            // on its way: at least as late as it left its last stop, and
                            // later still once its next stop is overdue
                            let next_arrive = tt
                                .stations
                                .iter()
                                .find(|s| (d.time + s.1 - next).abs() < 0.5)
                                .map(|s| d.time + s.0)
                                .unwrap_or(next);
                            let late = if r.dwelling {
                                r.late
                            } else {
                                r.late.max(now - next_arrive)
                            };
                            (arrive + late).max(now)
                        }
                    },
                    // not on the road (still to come, or where no tiles are loaded): on time
                    None if leave < now => continue,
                    None => arrive.max(now),
                };
                let terminus = &self.data.trips[d.trip].terminus;
                let hof = self
                    .depots
                    .get(&d.ai_group.to_ascii_lowercase())
                    .and_then(|v| v.iter().find_map(|x| x.2.as_deref()));
                list.push((expected, self.display_line(i), terminus_text(hof, terminus), (leave - arrive).max(0.0)));
            }
        }
        // the player's bus
        if let Some(duty) = duty {
            // a trip not begun leaves on time at the earliest (the bus waits at its
            // first stop), one under way arrives as early or late as it runs
            let delay = duty.delay(now);
            let lateness = if duty.left_late.is_some() || duty.at_stop {
                delay
            } else {
                delay.max(0.0)
            };
            for (ti, trip) in duty.trips.iter().enumerate().skip(duty.trip_index) {
                if trip.departure > now + BOARD_AHEAD {
                    break;
                }
                for (k, s) in trip
                    .stops
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.object_id == stop && s.stops)
                {
                    let current = ti == duty.trip_index;
                    let expected = if current
                        && (k < duty.next_stop || duty.done && k + 1 < trip.stops.len())
                    {
                        continue;
                    } else if current && k == duty.next_stop && duty.at_stop {
                        now
                    } else if current {
                        (s.arr + lateness).max(now)
                    } else {
                        (s.arr + delay.max(0.0)).max(now)
                    };
                    list.push((
                        expected,
                        trip.line.trim().to_string(),
                        terminus_text(player_hof, &trip.terminus),
                        (s.dep - s.arr).max(0.0),
                    ));
                }
            }
        }
        list
    }

    /// Make the departure boards of the stops whose displays are near
    /// (`World::timetable_boards`), and hand the scenery the time of day. The boards are
    /// made at most once a second: the buses due at each stop in the next two hours,
    /// soonest first - the timetable buses with the delay they run with, and the player's.
    pub fn update_boards(
        &mut self,
        world: &World,
        traffic: Option<&Traffic>,
        duty: Option<&PlayerDuty>,
        player_hof: Option<&omsi_vehicle::Hof>,
        clock: &omsi_sim::SimClock,
    ) {
        let now = clock.time;
        let mut boards = world.timetable_boards.lock();
        boards.clock = Some(clock.clone());
        if (now - self.boards_made).abs() < 1.0 || (boards.wanted.is_empty() && boards.wanted_names.is_empty()) {
            return;
        }
        self.boards_made = now;
        // the timetable buses on the road: departure -> where they are in their trip (a
        // train runs its track without stops of its own and is taken as on time)
        let mut on_road: HashMap<usize, OnRoad> = HashMap::new();
        if let Some(t) = traffic {
            for car in &t.cars {
                let Some(&i) = self.car_departure.get(&car.id) else {
                    continue;
                };
                if trip_stations(&self.data.trips[self.departures[i].trip]).is_empty() {
                    continue;
                }
                let next = car.bus.as_ref().and_then(|b| b.stops.front()).map(|s| s.depart).or_else(|| {
                    self.running.iter().find(|r| r.car == car.id).and_then(|r| {
                        r.stations
                            .iter()
                            .zip(&r.served)
                            .find(|(_, s)| !**s)
                            .map(|(st, _)| st.1)
                    })
                });
                on_road.insert(
                    i,
                    OnRoad {
                        next,
                        dwelling: car.at_stop(),
                        late: car.bus.as_ref().map(|b| b.delay).unwrap_or(0.0).max(0.0),
                    },
                );
            }
        }
        let wanted = boards.wanted.clone();
        let mut made = HashMap::new();
        for stop in wanted {
            let mut list = self.stop_list(stop, now, &on_road, duty, player_hof);
            list.sort_by(|a, b| a.0.total_cmp(&b.0));
            list.truncate(8);
            if omsi_cfg::env::var_os("OMSI_DEBUG_BOARDS").is_some() {
                log::info!(
                    "board of stop {stop} at {:.0} s: {:?}",
                    now,
                    list.iter()
                        .map(|(t, l, d, _)| format!("{l} {d} in {:.1} min", (t - now) / 60.0))
                        .collect::<Vec<_>>()
                );
            }
            made.insert(stop, list.into_iter().map(|(t, l, d, _)| (l, d, t)).collect());
        }
        boards.by_stop = made;
        // the departures the pages asked for by stop name (`omsi.getDepartures`): the next two
        // hours, at most 20, as (line, destination, timestamp)
        let mut departures = std::collections::HashMap::new();
        for key in boards.wanted_names.clone() {
            let mut ids: Vec<i64> = self
                .data
                .bus_stops
                .iter()
                .filter(|b| b.name.trim().eq_ignore_ascii_case(&key))
                .map(|b| b.object_id)
                .collect();
            ids.sort_unstable();
            ids.dedup();
            let mut list: Vec<(f64, String, String)> = Vec::new();
            for id in ids {
                for (expected, line, terminus, dwell) in self.stop_list(id, now, &on_road, duty, player_hof) {
                    let leaves = expected + dwell;
                    if leaves <= now + BOARD_AHEAD {
                        list.push((leaves, line, terminus));
                    }
                }
            }
            list.sort_by(|a, b| a.0.total_cmp(&b.0));
            list.truncate(MAX_PAGE_DEPARTURES);
            departures.insert(
                key,
                list.into_iter()
                    .map(|(t, l, d)| (l, d, omsi_sim::vehicle_api::timestamp(clock, t)))
                    .collect(),
            );
        }
        boards.departures = departures;
        boards.departures_gen = boards.departures_gen.wrapping_add(1);
    }

    /// The line a departure's displays show: its trip's own (" 5"), else the timetable
    /// line's name.
    /// The name of station `i` (object `id`) of `trip`, as the map's stop calls it.
    fn station_name(&self, trip: &omsi_timetable::Trip, i: usize, id: i64) -> String {
        self.data
            .bus_stops
            .iter()
            .find(|b| b.object_id == id)
            .map(|b| b.name.clone())
            .filter(|n| !n.trim().is_empty())
            // (the trip file names its stations too: a stop whose object is not
            // among the map's known stops - Novi Sad's, on tiles not loaded yet -
            // had no name on the navigator, the HUD and in the log)
            .or_else(|| {
                trip.stations
                    .is_empty()
                    .then(|| {
                        trip.stations_legacy
                            .get(i)
                            .and_then(|r| r.get(2))
                            .map(|n| n.trim().to_string())
                    })
                    .flatten()
            })
            .unwrap_or_default()
    }

    /// The station names of trip `ti`, for picking its route in the depot file.
    fn trip_stop_names(&self, ti: usize) -> Vec<String> {
        let trip = &self.data.trips[ti];
        trip_stations(trip)
            .iter()
            .enumerate()
            .map(|(i, id)| self.station_name(trip, i, *id))
            .collect()
    }

    fn display_line(&self, i: usize) -> String {
        let d = &self.departures[i];
        let own = self.data.trips[d.trip].line.trim();
        if own.is_empty() {
            d.line.trim().to_string()
        } else {
            own.to_string()
        }
    }
}

/// "HH:MM" of a time of day in seconds.
pub(crate) fn hhmm(t: f64) -> String {
    format!("{:02}:{:02}", (t / 3600.0) as i32, ((t % 3600.0) / 60.0) as i32)
}

/// The trip the player picked: "HH:MM" - the first trip leaving at that minute or later -
/// or its number in the tour (1 = the first).
fn chosen_trip(trips: &[PlannedTrip], pick: &str) -> Option<usize> {
    if let Some((h, m)) = pick.split_once(':') {
        let (h, m) = (h.trim().parse::<f64>().ok()?, m.trim().parse::<f64>().ok()?);
        let at = h * 3600.0 + m * 60.0;
        return trips.iter().position(|t| t.departure >= at - 30.0);
    }
    let n = pick.parse::<usize>().ok()?;
    (n >= 1 && n <= trips.len()).then(|| n - 1)
}

/// The trip of a duty that fits the time of day: the one under way, else the next to leave
/// (the last one once all are over).
fn starting_trip(trips: &[PlannedTrip], now: f64) -> usize {
    trips.iter().position(|t| t.end > now).unwrap_or_else(|| {
        // every trip over for today: a tour after midnight (the night line 13N's runs from
        // 0:49) picked in the evening is tonight's, and starts at its first trip
        match trips.first() {
            Some(first) if first.departure + DAY - now < DAY / 2.0 => 0,
            _ => trips.len().saturating_sub(1),
        }
    })
}

const DAY: f64 = 86_400.0;

/// A 64-bit hash of a tour: its depot group (lower case), line and tour number.
fn tour_key_of(group: &str, line: &str, tour: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in group.bytes().chain([0]).chain(line.trim().bytes()).chain([0]).chain(tour.trim().bytes()) {
        h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
    }
    mix(h)
}

fn mix(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^ (h >> 33)
}

/// A vehicle file path compared as `car_use` writes it (`vehicles\MAN_SD200\MAN_SD77.bus`):
/// lower case with forward slashes.
fn norm_vehicle_path(p: &str) -> String {
    p.trim().replace('\\', "/").to_ascii_lowercase()
}

/// The tour mask bits `clock`'s date selects: (the weekday's or public holiday's, the school
/// holidays' or school days'). the original: bit 8 = runs in the school holidays,
/// bit 9 = runs on school days.
fn day_bits(calendar: &omsi_map::Calendar, clock: &omsi_sim::SimClock) -> (i32, i32) {
    let date = clock.date_code();
    let day_bit = if calendar.is_holiday(date) { 1 << 7 } else { 1 << clock.weekday() };
    let school_bit = if calendar.in_holiday_range(date) { 1 << 8 } else { 1 << 9 };
    (day_bit, school_bit)
}

/// What a bus's displays call its terminus: the depot file's first string for it (what the
/// IBIS shows, in capitals - the stock departure display's font has no small letters, and
/// the trip's "Bauernhof" came out as a lone "B"), else the timetable's name in capitals
/// (a train has no depot file).
fn terminus_text(hof: Option<&omsi_vehicle::Hof>, terminus: &str) -> String {
    let name = terminus.trim();
    hof.and_then(|h| {
        h.termini.iter().find(|t| {
            t.texture_id.trim().eq_ignore_ascii_case(name)
                || t.strings
                .iter()
                .any(|s| s.trim().eq_ignore_ascii_case(name))
        })
    })
        .and_then(|t| t.strings.first())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| name.to_uppercase())
}

/// Where a timetable bus on the road is in its trip, for the departure boards.
struct OnRoad {
    /// Timetable departure time of its next stop (None: it has served its last).
    next: Option<f64>,
    /// Standing at that stop.
    dwelling: bool,
    /// How late it left its last stop (s).
    late: f64,
}

/// GetTTTerminusIndex as Omsi.exe answers it: the first depot terminus whose name is the
/// trip's terminus (the second [trip] line), else -1.
fn tt_terminus_index(hof: Option<&omsi_vehicle::hof::Hof>, terminus: &str) -> i32 {
    hof.and_then(|h| h.termini.iter().position(|t| t.texture_id == terminus)).map_or(-1, |i| i as i32)
}

impl PlayerDuty {
    pub fn trip(&self) -> &PlannedTrip {
        &self.trips[self.trip_index]
    }

    pub fn trip_done(&self) -> bool {
        self.done
    }

    /// Service/depot legs have no public line and use the HOF's
    /// `Betriebsfahrt` destination. They remain part of the duty, but the
    /// player's IBIS should use the next public leg while the bus is waiting.
    pub fn trip_for_ibis(&self) -> (&PlannedTrip, usize) {
        let current = self.trip();
        if !current.line.trim().is_empty() {
            return (
                current,
                self.next_stop.min(current.stops.len().saturating_sub(1)),
            );
        }
        let next = self
            .trips
            .iter()
            .skip(self.trip_index + 1)
            .find(|trip| !trip.line.trim().is_empty());
        match next {
            Some(trip) => (trip, 0),
            None => (
                current,
                self.next_stop.min(current.stops.len().saturating_sub(1)),
            ),
        }
    }

    /// Whether the current trip changed since the last call (the IBIS wants the new one).
    pub fn take_trip_change(&mut self) -> bool {
        std::mem::take(&mut self.trip_changed)
    }

    /// Places of stops the timetable did not know (their tiles were not loaded when the duty
    /// was made): the navigator reads the whole map.
    pub fn learn_places(&mut self, places: &HashMap<i64, glam::DVec3>) {
        for trip in &mut self.trips {
            for s in &mut trip.stops {
                if s.position.is_none() {
                    s.position = places.get(&s.object_id).copied();
                }
            }
            // (a stop that only now has a place gives its neighbours their direction)
            trip.set_dirs();
        }
    }

    /// The places of the stops ahead as the map has them now (`World::object_positions`):
    /// a stop whose tile was not loaded when the duty began - and that no index placed -
    /// gets its place once the tile comes, and one placed roughly (an object hung on
    /// another) its exact one. A stop without a place was never reached: the next stop
    /// stayed on it for the rest of the trip (#975) and the map left it out (#1014).
    pub fn learn_loaded(&mut self, positions: &HashMap<i64, (glam::DVec3, [f64; 3])>) {
        // (the trip under way and the next: the later ones learn theirs when they come)
        let from = self.trip_index;
        for trip in self.trips[from..].iter_mut().take(2) {
            let mut changed = false;
            for s in &mut trip.stops {
                if let Some((p, _)) = positions.get(&s.object_id) {
                    if s.position != Some(*p) {
                        s.position = Some(*p);
                        changed = true;
                    }
                }
            }
            if changed {
                trip.set_dirs();
            }
        }
    }

    /// Time to drive from `pos` to `to` (s), roughly: roads are longer than the straight
    /// line, a bus in town makes some 25 km/h, and it takes a minute or two to get going.
    fn approach_time(pos: glam::DVec3, to: glam::DVec3) -> f64 {
        let d = (to - pos).truncate().length();
        if d < AT_STOP {
            return 0.0;
        }
        d * 1.35 / 7.0 + 60.0
    }

    /// Where the bus starts: at the stop of the trip under way it stands at; else with the
    /// first trip of the tour whose first stop it can reach before that trip leaves (a
    /// duty picked for 08:00 with the bus in the depot used to start with the trip under
    /// way at 08:00, led the driver to whatever stop that trip was due at next - halfway
    /// along the line - and ran late from the first second). When no trip of the tour
    /// can be reached in time any more, the last one is driven from its first stop that
    /// can (else its first stop), late as that is.
    fn place(&mut self, pos: glam::DVec3, now: f64) {
        let trip = &self.trips[self.trip_index];
        // of two stops within AT_STOP of the bus - the two sides of a street on a circular
        // route - the one the bus drives the way the trip runs through it, else the nearer
        let fwd = forward_of(self.heading);
        let near: Vec<(usize, f64)> = trip
            .stops
            .iter()
            .enumerate()
            .filter_map(|(k, s)| s.position.map(|p| (k, (p - pos).length())))
            .filter(|(_, d)| *d < AT_STOP)
            .collect();
        let nearest = |v: Vec<(usize, f64)>| v.into_iter().min_by(|a, b| a.1.total_cmp(&b.1));
        let near = nearest(near.iter().copied().filter(|&(k, _)| trip.stops[k].dir.takes(fwd)).collect()).or_else(|| nearest(near));
        if near.is_none() && !self.picked {
            let reachable = (self.trip_index..self.trips.len()).find(|&k| {
                let t = &self.trips[k];
                let first = t.stops.first().and_then(|s| s.position);
                // (a first stop nobody knows the place of: ten minutes)
                let need = first.map(|p| Self::approach_time(pos, p)).unwrap_or(600.0);
                t.departure >= now + need
            });
            match reachable {
                Some(k) if k != self.trip_index => {
                    log::info!(
                        "duty: trip {} ({}) cannot be reached in time from here; starting with trip {} ({}) at {}",
                        self.trip_index + 1,
                        trip.name,
                        k + 1,
                        self.trips[k].name,
                        hhmm(self.trips[k].departure)
                    );
                    self.set_trip(k);
                    self.trip_changed = true;
                }
                Some(_) => {}
                None => {
                    let t = &self.trips[self.trip_index];
                    self.next_stop = t
                        .stops
                        .iter()
                        .position(|s| s.stops && s.position.map(|p| s.arr >= now + Self::approach_time(pos, p)).unwrap_or(false))
                        .unwrap_or(0);
                    log::info!(
                        "duty: no trip of the tour can be reached in time; trip {} ({}) from stop {} '{}'",
                        self.trip_index + 1,
                        t.name,
                        self.next_stop,
                        t.stops.get(self.next_stop).map(|s| s.name.as_str()).unwrap_or("")
                    );
                    return;
                }
            }
        }
        let trip = &self.trips[self.trip_index];
        if trip.departure >= now {
            log::info!(
                "duty: trip {} ({}) leaves {} at {:.0} s",
                self.trip_index + 1,
                trip.name,
                trip.stops.first().map(|s| s.name.as_str()).unwrap_or(""),
                trip.departure
            );
            return;
        }
        self.next_stop = match near {
            Some((k, _)) => k,
            // a trip the player picked is driven from its first stop, late as it may be
            None if self.picked => self.next_stop,
            None => trip
                .stops
                .iter()
                .position(|s| s.arr >= now)
                .unwrap_or(trip.stops.len().saturating_sub(1)),
        };
        // standing at a stop of it, the bus is on its way (as if it had left the stop before
        // on time); elsewhere the duty goes on with the next trip when that is due
        self.left_late = near.map(|_| 0.0);
        log::info!(
            "duty: the bus starts {} trip {} ({}) under way, next stop {} '{}'",
            if near.is_some() {
                "at a stop of"
            } else {
                "away from the stops of"
            },
            self.trip_index + 1,
            trip.name,
            self.next_stop,
            trip.stops
                .get(self.next_stop)
                .map(|s| s.name.as_str())
                .unwrap_or("")
        );
    }

    /// The trip a bus placed at its first stop starts with: the one under way or picked,
    /// else the first to leave from now on (the last when all have left).
    pub fn start_trip(&self, now: f64) -> usize {
        if self.picked {
            return self.trip_index;
        }
        (self.trip_index..self.trips.len())
            .find(|&k| self.trips[k].departure >= now)
            .unwrap_or(self.trip_index)
    }

    /// The duty starts with trip `k` at its stop `stop` (`duty_start`: the stops before it
    /// cannot be reached by road): that trip is the duty's first, driven from there.
    pub fn start_at(&mut self, k: usize, stop: usize) {
        if k < self.trips.len() {
            self.set_trip(k);
            self.picked = true;
            self.trip_changed = true;
            self.next_stop = stop.min(self.trips[k].stops.len().saturating_sub(1));
        }
    }

    /// Like `start_at`, for a bus that stays where it is (the stop was chosen in the menu,
    /// the bus is not put there): the first update does not look where the bus stands and
    /// does not move the chosen stop to one it happens to be near.
    pub fn start_at_here(&mut self, k: usize, stop: usize) {
        self.start_at(k, stop);
        self.placed = true;
    }

    /// Resume the saved trip at its saved ordinal, without choosing a fresh starting
    /// point or asking the device to retype its already restored programming. Past the
    /// first stop the bus is on its way, as if it had left the stop before on time (as
    /// `place` has it): `catch_up` may then look as far as the last stop and `delay`
    /// counts from there. Saved at the last stop, the trip is over.
    pub fn restore_progress(&mut self, stop: usize, pos: glam::DVec3) {
        self.served_terminus = None;
        let last = self.trip().stops.len().saturating_sub(1);
        self.next_stop = stop.min(last);
        self.left_late = (self.next_stop > 0).then_some(0.0);
        self.done = self.next_stop == last
            && self.trip().stops[last].position.is_some_and(|p| (p - pos).length() < AT_STOP);
        self.placed = true;
        self.picked = true;
        self.trip_changed = false;
    }

    /// The duty of a resumed situation: at its saved stop (an older save without one is
    /// placed by where the bus stands), with the timetable on the host before the first
    /// script frame.
    pub fn resume(&mut self, bus: &mut omsi_sim::VehicleInstance, day_time: f64, saved_stop: Option<usize>) {
        match saved_stop {
            Some(stop) => self.restore_progress(stop, bus.position),
            None => {
                self.update(bus, day_time);
            }
        }
        self.restore_host(bus, day_time);
    }

    /// A page sets the stop the duty goes on with (`omsi.setNextStop`), forwards or
    /// backwards: skipped stops count as not served, and going back makes the stops from
    /// `stop` on due again. Not once the trip's last stop is reached (`done`), unless the
    /// page goes back, which reopens the trip.
    pub fn skip_to(&mut self, stop: usize) -> bool {
        let last = self.trip().stops.len().saturating_sub(1);
        let stop = stop.min(last);
        log::debug!("duty: page asks for stop {stop} (next {}, at_stop {}, done {})", self.next_stop, self.at_stop, self.done);
        if stop == self.next_stop && !self.done {
            return false;
        }
        if self.done && stop >= self.next_stop {
            return false;
        }
        let back = stop < self.next_stop;
        self.next_stop = stop;
        self.at_stop = false;
        self.served_terminus = None;
        self.arrived_late = None;
        if back {
            self.done = false;
            self.held_back = true;
        }
        true
    }

    fn set_trip(&mut self, index: usize) {
        self.trip_index = index;
        self.next_stop = 0;
        self.at_stop = false;
        self.done = false;
        self.served_terminus = None;
        self.left_late = None;
        self.held_back = false;
        self.trip_changed = true;
        self.picked = false;
    }

    /// How late the bus is (s; negative = early), as the IBIS shows it: at a stop against
    /// its departure there, on the way at least as late as it left the last stop and later
    /// once the next one is overdue, and at the end of a trip against the next trip's start.
    pub fn delay(&self, now: f64) -> f64 {
        let now = self.duty_time(now);
        let trip = self.trip();
        if self.done {
            if let Some(next) = self.trips.get(self.trip_index + 1) {
                return now - next.departure;
            }
        }
        let Some(stop) = trip.stops.get(self.next_stop) else {
            return 0.0;
        };
        if self.at_stop {
            return now - stop.dep;
        }
        let due = now - stop.arr;
        self.left_late.map(|l| l.max(due)).unwrap_or(due)
    }

    /// The clock's time of day as the duty counts it: the day before or after when that is
    /// nearer the trip under way, so a duty across midnight (picked at 23:00 for trips from
    /// 0:49, or running from 23:40 into the night) is neither 22 hours late nor early.
    fn duty_time(&self, day_time: f64) -> f64 {
        let t = self.trip();
        let centre = (t.departure + t.end) / 2.0;
        [day_time - DAY, day_time, day_time + DAY]
            .into_iter()
            .min_by(|a, b| (a - centre).abs().total_cmp(&(b - centre).abs()))
            .unwrap_or(day_time)
    }

    /// Advance the duty and feed the vehicle host's timetable callbacks. Returns how late
    /// the bus left a stop, at the moment it leaves it (negative = early), which is what
    /// the personnel file counts.
    /// Returns, when the bus has just left a stop it had to serve, how late it arrived
    /// there and how late it left (seconds; negative: early).
    pub fn update(&mut self, bus: &mut omsi_sim::VehicleInstance, day_time: f64) -> Option<(f64, f64)> {
        let day_time = self.duty_time(day_time);
        self.heading = bus.heading;
        let served = self.advance(bus.position, day_time);
        if self.done
            && self.at_stop
            && bus.physics.velocity_kmh().abs() < 0.36
            && Self::doors_open(bus)
        {
            self.served_terminus = self.trip().stops.last().and_then(|stop| stop.position);
        }
        if self.trip_index + 1 < self.trips.len()
            && self
                .served_terminus
                .is_some_and(|stop| (bus.position - stop).length() >= 60.0)
        {
            let terminus = self.served_terminus.unwrap();
            self.set_trip(self.trip_index + 1);
            // This trip has begun, however early it is. Do not place it again or skip it
            // as an unbegun trip on a subsequent update.
            self.picked = true;
            self.left_late = Some(day_time - self.trip().departure);
            if self
                .trip()
                .stops
                .first()
                .and_then(|stop| stop.position)
                .is_some_and(|start| (start - terminus).length() < AT_STOP)
            {
                self.next_stop = 1.min(self.trip().stops.len().saturating_sub(1));
            }
            self.advance(bus.position, day_time);
        }
        self.feed_host(bus, day_time);
        served
    }

    fn doors_open(bus: &omsi_sim::VehicleInstance) -> bool {
        let mut reports_passenger_doors = false;
        let mut passenger_door_open = false;
        for i in 0..8 {
            for kind in ["Entry", "Exit"] {
                let name = format!("PAX_{kind}{i}_Open");
                if bus.has_script_var(&name)
                    || bus
                        .ty
                        .program
                        .var(&name)
                        .is_some_and(|id| bus.ty.program.stores(id))
                {
                    reports_passenger_doors = true;
                    passenger_door_open |= bus.var(&name).unwrap_or(0.0) > 0.5;
                }
            }
        }
        if reports_passenger_doors {
            passenger_door_open
        } else {
            (0..8).any(|i| {
                bus.var(&format!("door_{i}"))
                    .or_else(|| bus.var(&format!("door{i}")))
                    .unwrap_or(0.0)
                    > 0.5
            })
        }
    }

    /// Supply restored timetable data before the first resumed script frame.
    pub fn restore_host(&self, bus: &mut omsi_sim::VehicleInstance, day_time: f64) {
        self.feed_host(bus, day_time);
        bus.host.schedule_active = 1.0;
        bus.set_var("schedule_active", 1.0);
    }

    fn feed_host(&self, bus: &mut omsi_sim::VehicleInstance, day_time: f64) {
        let delay = self.delay(day_time);
        let trip = &self.trips[self.trip_index];
        let host = &mut bus.host;
        host.tt_line = trip.line.clone();
        host.tt_stops = trip
            .stops
            .iter()
            .map(|s| (s.name.clone(), s.arr as f32, s.dep as f32))
            .collect();
        host.tt_stop_ids = trip.stops.iter().map(|s| s.object_id).collect();
        host.tt_busstop_index = self.next_stop as i32;
        host.tt_terminus_index = tt_terminus_index(host.hof.as_deref(), &trip.terminus);
        host.tt_delay = delay as f32;
    }

    /// The bus came to a later stop of the trip than the one it is due at (it drove past
    /// some). Which stop that is cannot be told by the distance alone: a circular route, or
    /// one that turns back, calls at the same place twice, and its two stops there stand a
    /// few metres apart, so a bus at one is within [`AT_STOP`] of the other as well - the
    /// duty jumped from stop 2 to stop 18 and 3-17 were never served (#254). Three things
    /// have to agree: the trip runs through the stop the way the bus heads ([`StopDir`]);
    /// the bus stands nearer to it than to the stop it is due at; and it has driven away
    /// from the stop it served last.
    fn catch_up(&mut self, pos: glam::DVec3, fwd: glam::DVec2) {
        if self.held_back {
            return;
        }
        let trip = &self.trips[self.trip_index];
        let last = trip.stops.len().saturating_sub(1);
        let upto = if self.left_late.is_some() { trip.stops.len() } else { last };
        if self.next_stop + 1 >= upto {
            return;
        }
        let of = |k: usize| -> Option<f64> { trip.stops.get(k).and_then(|s| s.position).map(|p| (p - pos).length()) };
        // still at the stop it served: too early to look for a later one
        if let Some(k) = self.next_stop.checked_sub(1) {
            if of(k).is_some_and(|d| d <= AT_STOP) {
                return;
            }
        }
        let here = of(self.next_stop);
        let mut best: Option<(usize, f64)> = None;
        for k in self.next_stop + 1..upto {
            let Some(d) = of(k) else { continue };
            if d >= AT_STOP || here.is_some_and(|h| d >= h) || !trip.stops[k].dir.takes(fwd) {
                continue;
            }
            if best.is_none_or(|(_, b)| d < b) {
                best = Some((k, d));
            }
        }
        let Some((k, _)) = best else { return };
        log::info!(
            "duty: trip {}: {} stop(s) passed without stopping, the bus at stop {} '{}' (it was due at {})",
            trip.name,
            k - self.next_stop,
            k + 1,
            trip.stops[k].name.trim(),
            self.next_stop + 1
        );
        self.next_stop = k;
    }

    /// The duty's progress with the bus at `pos` (see [`PlayerDuty::update`]).
    fn advance(&mut self, pos: glam::DVec3, day_time: f64) -> Option<(f64, f64)> {
        if !self.placed {
            // (stops beyond the loaded tiles have no place yet: a few seconds for the
            // navigator's map, unless the bus stands at a stop of its trip)
            let first = *self.first_update.get_or_insert(day_time);
            let at_a_stop = self.trip().stops.iter().any(|s| s.position.map(|p| (p - pos).length() < AT_STOP).unwrap_or(false));
            let known = self.trips[self.trip_index..].iter().all(|t| t.stops.first().map(|s| s.position.is_some()).unwrap_or(true));
            if !(at_a_stop || known || (day_time - first).abs() > 12.0) {
                return None;
            }
            self.placed = true;
            self.place(pos, day_time);
        }
        // on to the next trip a minute before it leaves, once this one is over, was never
        // begun, or was given up half an hour ago
        while self.trip_index + 1 < self.trips.len()
            && self.trips[self.trip_index + 1].departure - 60.0 <= day_time
        {
            let given_up = day_time > self.trip().end + 1800.0;
            let unbegun = self.left_late.is_none() && !self.picked;
            // on the trip's last leg and standing at the next trip's first stop: this trip is
            // over even when its last stop was never reached within AT_STOP (a terminus
            // whose stop object lies away from where the buses stand, or that has no place
            // on the map). Before, the duty stayed on the old trip until half an hour after
            // its end: the IBIS kept the old terminus, the people at the new trip's stops
            // waited for another bus, and they boarded only once that half hour was up -
            // somewhere along the route.
            let last = self.trip().stops.len().saturating_sub(1);
            // (at the last stop itself it is reached the usual way, its arrival counted)
            let at_last = self.trip().stops.get(last).and_then(|s| s.position).is_some_and(|p| (p - pos).length() < AT_STOP);
            let at_next_start = self.next_stop >= last
                && !at_last
                && self.trips[self.trip_index + 1].stops.first().and_then(|s| s.position).is_some_and(|p| (p - pos).length() < AT_STOP);
            if !(self.done || unbegun || given_up || at_next_start) {
                break;
            }
            self.set_trip(self.trip_index + 1);
            log::info!(
                "duty: trip {} {} to {}",
                self.trip_index + 1,
                self.trip().name,
                self.trip().terminus
            );
        }
        let mut served = None;
        let trip = &self.trips[self.trip_index];
        let last = trip.stops.len().saturating_sub(1);
        // the bus reached a later stop of the trip (skipped stops); before it has left a
        // stop of the trip, not its last one (where the tour's previous trip may end)
        if !self.at_stop {
            self.catch_up(pos, forward_of(self.heading));
        }
        let trip = &self.trips[self.trip_index];
        // stop progress by proximity
        if let Some(stop) = trip.stops.get(self.next_stop) {
            if let Some(p) = stop.position {
                let d = (p - pos).length();
                if d < AT_STOP {
                    self.held_back = false;
                    if !self.at_stop {
                        self.arrived_late = Some(day_time - stop.arr);
                    }
                    self.at_stop = true;
                    if self.next_stop == last {
                        self.done = true;
                    }
                } else if self.at_stop && d > LEFT_STOP {
                    self.at_stop = false;
                    let late = day_time - stop.dep;
                    self.left_late = Some(late);
                    // (OMSI counts a stop only with its arrival: the original)
                    if let (true, Some(arrived)) = (stop.stops, self.arrived_late.take()) {
                        served = Some((arrived, late));
                    }
                    self.next_stop = (self.next_stop + 1).min(last);
                    log::debug!("duty: left stop, next stop now {}", self.next_stop);
                }
            }
        }
        served
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The row OMSI's AI bus is given: the first whose ident is the destination, whatever
    /// the codes' order; of equally loose matches the first as well.
    #[test]
    fn a_tour_bus_takes_a_trip_on_only_at_its_start() {
        let route = [10, 11, 12, 13, 14, 15, 16];
        assert_eq!(tour_entry(&route, 10), Some(0));
        assert_eq!(tour_entry(&route, 13), Some(3));
        // the same trip again: the bus stands on its last lane
        assert_eq!(tour_entry(&route, 16), None);
        assert_eq!(tour_entry(&route, 99), None);
    }

    #[test]
    fn a_stop_is_no_target_of_itself() {
        // a circular line: from A round to A; B has two platforms of one name
        let names = |id: i64| match id {
            1 => "A".to_string(),
            2 | 3 => "B".to_string(),
            _ => "C".to_string(),
        };
        let t = station_targets([(vec![1, 2, 4, 3, 1], "A".to_string())].into_iter(), names);
        let of = |id: i64| t[&id].iter().map(|x| x.0.as_str()).collect::<Vec<_>>();
        assert_eq!(of(1), ["B", "C"]);
        assert_eq!(of(2), ["C", "A"]);
        assert_eq!(of(4), ["B", "A"]);
        assert_eq!(of(3), ["A"]);
        assert!(t[&1].iter().all(|x| x.1.contains("A")));
    }

    #[test]
    fn a_terminus_is_the_first_row_of_its_name() {
        let t = |code: i32, id: &str, s: &[&str]| omsi_vehicle::hof::Terminus { code, texture_id: id.into(), terminus_stop: Some(id.into()), all_exit: false, strings: s.iter().map(|x| x.to_string()).collect() };
        let hof = omsi_vehicle::Hof { termini: vec![t(0, "Depot", &[]), t(3, "61-Other", &["61-MaoFangChang"]), t(4, "61-Third", &[]), t(1, "61-MaoFangChang", &["61-MaoFangChang"]), t(2, "Wickenberg Nord", &[])], ..Default::default() };
        assert_eq!(super::find_terminus(&hof, "61-MaoFangChang"), Some(3));
        assert_eq!(super::find_terminus(&hof, "  61-MaoFangChang "), Some(3));
        let hof = omsi_vehicle::Hof { termini: vec![t(0, "A", &["Wickenberg"]), t(1, "B", &["Wickenberg"])], ..Default::default() };
        assert_eq!(super::find_terminus(&hof, "wickenberg"), Some(0));
    }

    #[test]
    fn complex_line_keeps_letter_suffix() {
        assert_eq!(complex_line_text("5", 5.0), "005  ");
        assert_eq!(complex_line_text("5E", 5.0), "   5E");
        assert_eq!(line_suffix_from_text("5E"), 10);
        assert_eq!(line_suffix_from_text("5N"), 4);
        assert_eq!(line_suffix_from_text("5S"), 23);
        assert_eq!(line_code_from_text("5E", Some(505)), Some(510));
        assert_eq!(line_code_from_text("5", Some(505)), Some(500));
    }

    /// #459: a four-digit line keeps its number and gets no suffix from its route code.
    #[test]
    fn four_digit_line_keeps_its_number() {
        assert_eq!(line_code_from_text("7110", Some(711001)), Some(711000));
        assert_eq!(line_code_from_text("7110", None), Some(711000));
        assert_eq!(line_code_from_text("7110-10", Some(711010)), Some(711000));
        assert_eq!(line_code_from_text("1234E", None), Some(123410));
        assert_eq!(complex_line_text("7110", 7110.0), "7110  ");
    }

    /// #546: a letter-first line had no number, and the DL05's matrix blanks line 0.
    #[test]
    fn line_with_letter_prefix_keeps_its_number() {
        assert_eq!(line_code_from_text("X10", None), Some(1036));
        assert_eq!(line_code_from_text("X10", Some(51001)), Some(51036));
        assert_eq!(line_code_from_text("M41", Some(4101)), Some(4128));
        assert_eq!(line_code_from_text("N9", None), Some(935));
        assert_eq!(line_code_from_text("TML", Some(7601)), Some(7601));
        assert_eq!(line_suffix_from_text("X10"), 36);
        assert_eq!(line_number_digits("X10"), "10");
        assert_eq!(line_number_digits("5E"), "5");
    }

    #[test]
    fn berlin_5e_uses_its_real_terminus_when_no_hof_route_exists() {
        let path = std::path::Path::new("../../../OMSI 2 Original/Vehicles/MAN_SD202/Berlin.hof");
        let Ok(hof) = omsi_vehicle::Hof::load(path) else {
            return;
        };
        let target = ibis_target(&hof, "5E", "Fernbahnhof Spandau", &[], None).expect("5E target");
        assert_eq!(target.terminus_code, Some(232));
        assert_eq!(
            target.terminus_index,
            hof.termini.iter().position(|t| t.code == 232).unwrap() as i32
        );
        let target = ibis_target(&hof, "5E", "Spektefeld Schulzentrum", &[], None)
            .expect("5E shortened HOF target");
        assert_eq!(target.terminus_code, Some(233));
    }

    #[test]
    fn berlin_5e_does_not_turn_hof_route_505_into_s5() {
        let path =
            std::path::Path::new("../../../OMSI 2 Original/Vehicles/MAN_NL_NG/Spandau 89-11.hof");
        let Ok(hof) = omsi_vehicle::Hof::load(path) else {
            return;
        };
        let target = ibis_target(&hof, "5E", "Nervenklinik", &["U Rathaus Spandau"], None)
            .expect("5E route target");
        assert_eq!(target.route, Some(3));
        assert_eq!(target.suffix, 10);
    }

    #[test]
    fn stop_names_meet_in_any_order_and_spelling() {
        assert_eq!(stop_words("Nordstadt Bhf"), stop_words("Bhf. Nordstadt"));
        assert_eq!(stop_words("F_Kirchweg"), stop_words("Kirchweg"));
        assert_ne!(stop_words("Bhf Nordstadt"), stop_words("Nordstadt"));
        assert!(stop_words("").is_empty());
    }

    /// A made-up line 7 with six routes to Hafen, each telling one rule of `pick_route`.
    fn hafen_depot() -> omsi_vehicle::Hof {
        let mut hof = omsi_vehicle::Hof {
            termini: vec![omsi_vehicle::hof::Terminus {
                code: 100,
                strings: vec!["Hafen".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let routes: [(&str, &[&str]); 6] = [
            ("707", &["Markt", "Schule", "Park", "Ufer", "Hafen"]),
            ("701", &["Markt", "Schule", "Park", "Hafen"]),
            ("702", &["Bhf Nordstadt", "F_Kirchweg", "Markt", "Schule", "Park", "Hafen"]),
            ("703", &["Schule", "Park", "Hafen"]),
            ("705", &["Bhf Nordstadt", "Markt", "Rathaus", "Schule", "Park", "Hafen"]),
            ("706", &["Am Wald", "Kirchweg", "Markt", "Schule", "Park", "Hafen"]),
        ];
        for (code, stops) in routes {
            hof.info_trips.push(omsi_vehicle::hof::InfoTrip {
                code: code.into(),
                route: "100".into(),
                line: "7".into(),
                ..Default::default()
            });
            hof.info_busstop_lists
                .push(stops.iter().map(|s| s.to_string()).collect());
        }
        hof
    }

    #[test]
    fn the_route_follows_the_trips_stops() {
        let hof = hafen_depot();
        let route = |stops: &[&str]| {
            ibis_target(&hof, "7", "Hafen", stops, None)
                .expect("line 7 target")
                .route
        };
        // the depot file spells the first stop another way, and with a one-letter prefix
        assert_eq!(
            route(&["Nordstadt Bhf", "Kirchweg", "Markt", "Schule", "Park", "Hafen"]),
            Some(2)
        );
        // a short working gets its own route, not the long ones it is part of
        assert_eq!(route(&["Schule", "Park", "Hafen"]), Some(3));
        // of two routes from the trip's first stop, the one as long as the trip
        assert_eq!(route(&["Markt", "Schule", "Park", "Hafen"]), Some(1));
        // starting at the trip's first stop counts before one stop more of the trip: 705
        // has Rathaus too but begins at Bhf Nordstadt, so a route from Markt (707, as
        // long as the trip) is taken
        assert_eq!(
            route(&["Markt", "Rathaus", "Schule", "Park", "Hafen"]),
            Some(7)
        );
        // nothing known of the trip: the first route, as before
        assert_eq!(route(&[]), Some(7));
    }

    #[test]
    fn the_ibis_stands_at_the_trips_first_stop_on_a_route_that_begins_before_it() {
        let hof = hafen_depot();
        // no route begins at Kirchweg; 702 and 706 have the trip's stops after one more
        let stops = ["Kirchweg", "Markt", "Schule", "Park", "Hafen"];
        let target = ibis_target(&hof, "7", "Hafen", &stops, Some((0, stops[0])))
            .expect("line 7 target");
        assert_eq!(target.route, Some(2));
        assert_eq!(target.stop, 1);
    }

    #[test]
    fn where_a_bus_is_on_a_partly_loaded_route() {
        let key = |id: i64| {
            Some(LaneKey {
                tile: (0, 0),
                id,
                path: 0,
            })
        };
        let legs = [0, 0, 1, 1, 1, 2];
        let steps: Vec<Step> = legs
            .iter()
            .enumerate()
            .map(|(i, &leg)| Step {
                key: key(i as i64),
                leg,
                length: 0.0,
            })
            .collect();
        let slots = [
            Slot::Lane(10),
            Slot::Lane(11),
            Slot::Lane(12),
            Slot::Waiting,
            Slot::Lane(14),
            Slot::Absent,
        ];
        let est = [100.0, 100.0, 50.0, 70.0, 50.0, 0.0];
        // a layover bus stands at the start
        assert_eq!(step_at(&steps, &slots, &est, 0, 0.0), Some((0, 0.0)));
        // 17 m into leg 1: on its first lane, whose part of the route ends at the gap
        let (at, off) = step_at(&steps, &slots, &est, 1, 0.1).unwrap();
        assert!(at == 2 && (off - 17.0).abs() < 1e-9, "{at} {off}");
        assert_eq!(section_around(&slots, 2), (0, 3));
        // half way: on the step still to come - the bus has to wait
        assert_eq!(step_at(&steps, &slots, &est, 1, 0.5), Some((3, 35.0)));
        // near the end of the leg: after the gap
        let (at, off) = step_at(&steps, &slots, &est, 1, 0.9).unwrap();
        assert_eq!(at, 4);
        assert!((off - 33.0).abs() < 1e-9);
        assert_eq!(section_around(&slots, 4), (4, 6));
        // a leg of absent steps only, and a leg without steps: past the end
        assert_eq!(step_at(&steps, &slots, &est, 2, 0.5), None);
        assert_eq!(step_at(&steps, &slots, &est, 3, 0.5), None);
        // a leg without a station link: at the start of the next leg
        let steps2: Vec<Step> = [0, 2, 2]
            .iter()
            .enumerate()
            .map(|(i, &leg)| Step {
                key: key(i as i64),
                leg,
                length: 0.0,
            })
            .collect();
        let slots2 = [Slot::Lane(1), Slot::Absent, Slot::Lane(3)];
        assert_eq!(
            step_at(&steps2, &slots2, &[10.0, 0.0, 10.0], 1, 0.5),
            Some((2, 0.0))
        );
        assert_eq!(section_around(&slots2, 2), (0, 3));
    }

    fn link(a: i64, b: i64) -> Option<f64> {
        // 100 m between neighbours, 300 m from 3 to 4
        Some(if (a, b) == (3, 4) { 300.0 } else { 100.0 })
    }

    #[test]
    fn trip_times_from_the_profile() {
        let stations = [1, 2, 3, 4, 5];
        // no manual times: the duration split by the link lengths
        let p = omsi_timetable::TripProfile {
            name: "p".into(),
            factor: 10.0,
            ..Default::default()
        };
        let t = TripTimes::new(&stations, Some(&p), &link);
        let arr: Vec<f64> = t.stations.iter().map(|s| s.0).collect();
        assert_eq!(arr, vec![0.0, 100.0, 200.0, 500.0, 600.0]);
        assert_eq!(t.duration, 600.0);
        // manual minutes win, the rest in between by length; a passed station stops nowhere
        let p = omsi_timetable::TripProfile {
            name: "p".into(),
            factor: 10.0,
            man_dep_time: vec![(0, 1.0), (1, 2.0)],
            man_arr_time: vec![(3, 6.0), (4, 9.0)],
            other_stopping: vec![(2, 2)],
        };
        let t = TripTimes::new(&stations, Some(&p), &link);
        assert_eq!(t.stations[0], (60.0, 60.0));
        assert_eq!(t.stations[1], (120.0, 120.0));
        // station 2 lies 100 m of the 400 m between the departure at 2 min and the arrival at 6
        assert!((t.stations[2].0 - 180.0).abs() < 1e-9, "{:?}", t.stations);
        assert_eq!(t.stations[3], (360.0, 360.0));
        assert_eq!(t.duration, 540.0);
        assert_eq!(t.stops, vec![true, true, false, true, true]);
        // a profile without stations keeps its duration (flights on a track)
        assert_eq!(
            TripTimes::new(
                &[],
                Some(&omsi_timetable::TripProfile {
                    factor: 25.0,
                    ..Default::default()
                }),
                &link
            )
                .duration,
            1500.0
        );
    }

    fn planned(departure: f64, stops: &[(f64, f64, f64)]) -> PlannedTrip {
        let stops: Vec<PlannedStop> = stops
            .iter()
            .enumerate()
            .map(|(i, &(x, arr, dep))| PlannedStop {
                object_id: i as i64,
                name: format!("s{i}"),
                arr,
                dep,
                position: Some(glam::DVec3::new(x, 0.0, 0.0)),
                dir: StopDir::default(),
                stops: true,
            })
            .collect();
        PlannedTrip {
            name: format!("t{departure}"),
            line: "5".into(),
            terminus: "T".into(),
            departure,
            end: stops.last().unwrap().arr,
            stops,
        }
    }

    /// A vehicle whose script drops its duty and destination while it has no timetable,
    /// and shows what the timetable callbacks tell it.
    fn timetable_test_vehicle() -> omsi_sim::VehicleInstance {
        timetable_test_vehicle_with_door("door_0")
    }

    fn timetable_test_vehicle_with_door(door: &str) -> omsi_sim::VehicleInstance {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "omsi_tt_restore_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("device.osc");
        let vars = dir.join("vars.txt");
        let strings = dir.join("strings.txt");
        std::fs::write(
            &vars,
            format!("duty\nobserved_stop\nobserved_delay\n{door}\n"),
        )
        .unwrap();
        std::fs::write(&strings, "destination\nobserved_line\n").unwrap();
        std::fs::write(
            &script,
            r#"
{frame}
(L.L.schedule_active) ! (M.V.GetTTBusstopCount) 0 = ||
{if}
0 (S.L.duty)
"" (S.$.destination)
{endif}
(M.V.GetTTLineString) (S.$.observed_line)
(M.V.GetTTBusstopIndex) (S.L.observed_stop)
(M.V.GetTTDelay) (S.L.observed_delay)
{end}
"#,
        )
        .unwrap();
        let program = omsi_script::compile(&omsi_script::CompileInput {
            scripts: vec![script],
            varlists: vec![vars],
            stringvarlists: vec![strings],
            builtin_vars: vec!["schedule_active".into()],
            ..Default::default()
        });
        assert!(program.errors.is_empty(), "{:?}", program.errors);
        let ty = Arc::new(omsi_sim::VehicleType {
            def: Default::default(),
            model: Default::default(),
            model_dir: dir.clone(),
            program: Arc::new(program),
            meshes: Vec::new(),
            paint_schemes: Vec::new(),
            texchanges: Vec::new(),
            wheel_meshes: Vec::new(),
            suspension_axles: Vec::new(),
            missing_packs: Vec::new(),
            mesh_bounds: Vec::new(),
            mesh_boxes: Vec::new(),
        });
        std::fs::remove_dir_all(dir).unwrap();
        omsi_sim::VehicleInstance::new(ty, omsi_sim::VehicleHost::new(Default::default()))
    }

    fn early_departure_duty() -> PlayerDuty {
        PlayerDuty {
            line: "5".into(),
            tour: "1".into(),
            trips: vec![
                planned(100.0, &[(0.0, 100.0, 100.0), (500.0, 200.0, 200.0)]),
                planned(600.0, &[(500.0, 600.0, 600.0), (1000.0, 700.0, 700.0)]),
                planned(900.0, &[(1000.0, 900.0, 900.0), (1500.0, 1000.0, 1000.0)]),
            ],
            trip_index: 0,
            first_trip: 0,
            next_stop: 1,
            at_stop: false,
            arrived_late: None,
            done: false,
            served_terminus: None,
            left_late: Some(0.0),
            held_back: false,
            placed: true,
            trip_changed: false,
            picked: true,
            first_update: None,
            heading: 90.0,
        }
    }

    #[test]
    fn serving_the_terminus_then_leaving_60m_starts_the_next_trip_early() {
        for door in ["door_0", "PAX_Exit0_Open"] {
            let mut duty = early_departure_duty();
            let mut bus = timetable_test_vehicle_with_door(door);
            bus.position.x = 500.0;
            bus.set_var(door, 1.0);
            duty.update(&mut bus, 150.0);
            assert_eq!(duty.trip_index, 0, "opening a door does not depart");
            bus.set_var(door, 0.0);
            bus.set_speed(5.0);
            bus.position.x = 559.9;
            assert_eq!(duty.update(&mut bus, 160.0), Some((-50.0, -40.0)));
            assert_eq!(duty.trip_index, 0, "less than 60 m away");
            bus.position.x = 560.0;
            duty.update(&mut bus, 161.0);
            assert_eq!((duty.trip_index, duty.next_stop), (1, 1));
            assert_eq!(bus.host.tt_busstop_index, 1);
            assert_eq!(bus.host.tt_stops[0].1, 600.0);
            assert_eq!(duty.delay(161.0), -439.0);
            assert!(duty.take_trip_change());
            duty.update(&mut bus, 162.0);
            assert!(!duty.take_trip_change());
            assert_eq!(duty.trip_index, 1, "advance exactly one trip");
            // The previous trip's door opening cannot finish the next trip too.
            bus.position.x = 1000.0;
            duty.update(&mut bus, 200.0);
            bus.position.x = 1060.0;
            duty.update(&mut bus, 210.0);
            assert_eq!(duty.trip_index, 1);
        }
    }

    #[test]
    fn early_trip_progress_requires_stopping_with_a_door_open_at_the_last_stop() {
        for (speed, open) in [(5.0, 1.0), (0.0, 0.0)] {
            let mut duty = early_departure_duty();
            let mut bus = timetable_test_vehicle();
            // Doors opened at another stop do not serve the terminus.
            bus.set_var("door_0", 1.0);
            duty.update(&mut bus, 140.0);
            bus.position.x = 500.0;
            bus.set_speed(speed);
            bus.set_var("door_0", open);
            duty.update(&mut bus, 150.0);
            bus.position.x = 560.0;
            bus.set_speed(5.0);
            bus.set_var("door_0", 0.0);
            duty.update(&mut bus, 160.0);
            assert_eq!(duty.trip_index, 0);
        }
    }

    #[test]
    fn waiting_at_the_terminus_still_uses_the_scheduled_changeover() {
        let mut duty = early_departure_duty();
        let mut bus = timetable_test_vehicle();
        bus.position.x = 500.0;
        bus.set_var("door_0", 1.0);
        duty.update(&mut bus, 150.0);
        duty.update(&mut bus, 539.0);
        assert_eq!(duty.trip_index, 0);
        duty.update(&mut bus, 540.0);
        assert_eq!((duty.trip_index, duty.next_stop), (1, 0));
        assert!(duty.served_terminus.is_none());
    }

    #[test]
    fn early_departure_does_not_run_past_the_end_of_the_duty() {
        let mut duty = early_departure_duty();
        duty.trips.truncate(1);
        let mut bus = timetable_test_vehicle();
        bus.position.x = 500.0;
        bus.set_var("door_0", 1.0);
        duty.update(&mut bus, 150.0);
        bus.position.x = 560.0;
        bus.set_speed(5.0);
        duty.update(&mut bus, 160.0);
        assert_eq!(duty.trip_index, 0);
        assert!(duty.done);
    }

    #[test]
    fn resumed_duty_keeps_its_trip_and_stop_before_the_first_script_frame() {
        let trips = vec![
            planned(100.0, &[(0.0, 100.0, 100.0), (500.0, 200.0, 220.0)]),
            planned(
                400.0,
                &[
                    (500.0, 400.0, 400.0),
                    (1000.0, 500.0, 520.0),
                    (1500.0, 600.0, 600.0),
                ],
            ),
            planned(800.0, &[(1500.0, 800.0, 800.0)]),
        ];
        let mut d = PlayerDuty {
            line: "5".into(),
            tour: "65104".into(),
            trips,
            trip_index: 0,
            first_trip: 0,
            next_stop: 0,
            at_stop: false,
            arrived_late: None,
            done: false,
            served_terminus: None,
            left_late: None,
            held_back: false,
            placed: false,
            trip_changed: false,
            picked: false,
            first_update: None,
            heading: 90.0,
        };
        // The same name appears twice. Its saved ordinal, rather than its name or the
        // bus's position far from any stop, selects the second occurrence.
        d.trips[1].stops[0].name = "Central".into();
        d.trips[1].stops[1].name = "Central".into();
        d.start_at(1, 0);
        d.restore_progress(1, glam::DVec3::new(-3500.0, 0.0, 0.0));
        assert_eq!(d.left_late, Some(0.0), "past its first stop the bus is on its way");
        assert!(
            !d.take_trip_change(),
            "resume must not request automatic reprogramming"
        );
        let mut v = timetable_test_vehicle();
        // Demonstrate that this device really loses its programming without callbacks.
        let vars = vec![("duty".into(), 65104.0)];
        let strings = vec![("destination".into(), "  Manual destination  ".into())];
        v.restore_script_state(&vars, &strings);
        v.update(0.02);
        assert_eq!(v.var("duty"), Some(0.0));
        v.restore_script_state(&vars, &strings);
        v.position = glam::DVec3::new(-3500.0, 0.0, 0.0);
        d.restore_host(&mut v, 542.0);
        v.update(0.02);
        assert_eq!(v.var("duty"), Some(65104.0));
        let dest = v.ty.program.str_var("destination").unwrap() as usize;
        assert_eq!(v.state.str_vars[dest], "  Manual destination  ");
        assert_eq!(v.var("observed_stop"), Some(1.0));
        assert_eq!(v.var("observed_delay"), Some(42.0));
        d.update(&mut v, 542.0);
        assert_eq!((d.trip_index, d.next_stop), (1, 1));
        assert_eq!(d.tour, "65104");
        d.restore_progress(usize::MAX, glam::DVec3::new(-3500.0, 0.0, 0.0));
        assert_eq!(d.next_stop, 2);
        assert!(!d.done, "away from the last stop the trip is not over yet");
        d.restore_progress(2, glam::DVec3::new(1500.0, 0.0, 0.0));
        assert!(d.done, "saved at the last stop, the trip is over");
        d.restore_progress(0, glam::DVec3::new(-3500.0, 0.0, 0.0));
        assert_eq!(d.next_stop, 0);
        assert_eq!(d.left_late, None);
        assert!(!d.done);
        // Ordinary duty updates retain their existing handling of schedule_active.
        v.host.schedule_active = 0.0;
        v.set_var("schedule_active", 0.0);
        d.update(&mut v, 542.0);
        assert_eq!(v.host.schedule_active, 0.0);
        assert_eq!(v.var("schedule_active"), Some(0.0));
    }

    #[test]
    fn terminus_index_is_the_depot_terminus_of_that_name() {
        let mut hof = omsi_vehicle::hof::Hof::default();
        for name in ["A", "B", "C"] {
            hof.termini.push(omsi_vehicle::hof::Terminus { texture_id: name.into(), ..Default::default() });
        }
        assert_eq!(tt_terminus_index(Some(&hof), "B"), 1);
        assert_eq!(tt_terminus_index(Some(&hof), "b"), -1);
        assert_eq!(tt_terminus_index(None, "B"), -1);
    }

    #[test]
    fn a_stop_placed_only_once_its_tile_loads_is_reached() {
        // stop 1's object was in no tile loaded when the duty began
        let mut t = planned(0.0, &[(0.0, 0.0, 0.0), (500.0, 100.0, 100.0), (1000.0, 200.0, 200.0)]);
        t.stops[1].position = None;
        let mut d = PlayerDuty {
            line: "5".into(),
            tour: "1".into(),
            trips: vec![t],
            trip_index: 0,
            first_trip: 0,
            next_stop: 0,
            at_stop: false,
            arrived_late: None,
            done: false,
            served_terminus: None,
            left_late: None,
            held_back: false,
            placed: true,
            trip_changed: false,
            picked: true,
            first_update: None,
            heading: 90.0,
        };
        d.advance(glam::DVec3::new(0.0, 0.0, 0.0), 0.0);
        d.advance(glam::DVec3::new(100.0, 0.0, 0.0), 10.0);
        assert_eq!(d.next_stop, 1);
        // its tile comes: the map has it now
        let mut positions = HashMap::new();
        positions.insert(1i64, (glam::DVec3::new(500.0, 0.0, 0.0), [0.0; 3]));
        d.learn_loaded(&positions);
        assert_eq!(d.trips[0].stops[1].position, Some(glam::DVec3::new(500.0, 0.0, 0.0)));
        d.advance(glam::DVec3::new(500.0, 0.0, 0.0), 100.0);
        d.advance(glam::DVec3::new(700.0, 0.0, 0.0), 130.0);
        assert_eq!(d.next_stop, 2);
    }

    #[test]
    fn the_next_trip_starts_at_its_first_stop_though_the_last_one_was_missed() {
        // trip 1 ends at x = 1000 (a stop object the bus never comes within 25 m of: it
        // stands at x = 1040, where trip 2 leaves from)
        let t1 = planned(0.0, &[(0.0, 0.0, 0.0), (500.0, 100.0, 100.0), (1000.0, 200.0, 200.0)]);
        let t2 = planned(400.0, &[(1040.0, 400.0, 400.0), (1500.0, 500.0, 500.0)]);
        let mut d = PlayerDuty {
            line: "5".into(),
            tour: "1".into(),
            trips: vec![t1, t2],
            trip_index: 0,
            first_trip: 0,
            next_stop: 0,
            at_stop: false,
            arrived_late: None,
            done: false,
            served_terminus: None,
            left_late: None,
            held_back: false,
            placed: true,
            trip_changed: false,
            picked: true,
            first_update: None,
            heading: 90.0,
        };
        d.advance(glam::DVec3::new(0.0, 0.0, 0.0), 0.0);
        d.advance(glam::DVec3::new(100.0, 0.0, 0.0), 10.0);
        d.advance(glam::DVec3::new(500.0, 0.0, 0.0), 100.0);
        d.advance(glam::DVec3::new(700.0, 0.0, 0.0), 130.0);
        assert_eq!(d.next_stop, 2);
        d.take_trip_change();
        // at trip 2's first stop a minute before it leaves: trip 2, and the IBIS is told
        d.advance(glam::DVec3::new(1040.0, 0.0, 0.0), 300.0);
        assert_eq!(d.trip_index, 0, "not before a minute ahead of the departure");
        d.advance(glam::DVec3::new(1040.0, 0.0, 0.0), 345.0);
        assert_eq!(d.trip_index, 1);
        assert!(d.take_trip_change());
        // a bus still on its way (not at trip 2's first stop) stays on trip 1
        let t1 = planned(0.0, &[(0.0, 0.0, 0.0), (500.0, 100.0, 100.0), (1000.0, 200.0, 200.0)]);
        let t2 = planned(400.0, &[(1040.0, 400.0, 400.0), (1500.0, 500.0, 500.0)]);
        let mut d = PlayerDuty {
            line: "5".into(),
            tour: "1".into(),
            trips: vec![t1, t2],
            trip_index: 0,
            first_trip: 0,
            next_stop: 2,
            at_stop: false,
            arrived_late: None,
            done: false,
            served_terminus: None,
            left_late: Some(0.0),
            held_back: false,
            placed: true,
            trip_changed: false,
            picked: true,
            first_update: None,
            heading: 90.0,
        };
        d.advance(glam::DVec3::new(800.0, 0.0, 0.0), 345.0);
        assert_eq!(d.trip_index, 0);
    }

    #[test]
    fn a_page_can_go_back_to_an_earlier_stop() {
        let trip = planned(
            0.0,
            &[
                (0.0, 0.0, 0.0),
                (100.0, 60.0, 60.0),
                (500.0, 120.0, 120.0),
                (1000.0, 200.0, 200.0),
            ],
        );
        let mut d = PlayerDuty {
            line: "5".into(),
            tour: "1".into(),
            trips: vec![trip],
            trip_index: 0,
            first_trip: 0,
            next_stop: 0,
            at_stop: false,
            arrived_late: None,
            done: false,
            served_terminus: None,
            left_late: None,
            held_back: false,
            placed: true,
            trip_changed: false,
            picked: true,
            first_update: None,
            heading: 90.0,
        };
        assert!(d.skip_to(2));
        assert_eq!(d.next_stop, 2);
        // back one stop: due again
        assert!(d.skip_to(1));
        assert_eq!(d.next_stop, 1);
        // the stop it is already heading for: nothing changes
        assert!(!d.skip_to(1));
        // the bus stands at stop 2: the duty does not jump forward again by itself
        d.advance(glam::DVec3::new(500.0, 0.0, 0.0), 100.0);
        assert_eq!(d.next_stop, 1);
        // from the last stop (done) back reopens the trip, forwards does not
        d.skip_to(3);
        d.advance(glam::DVec3::new(1000.0, 0.0, 0.0), 200.0);
        assert!(d.done);
        assert!(!d.skip_to(3));
        assert!(d.skip_to(2));
        assert!(!d.done);
        assert_eq!(d.next_stop, 2);
    }

    #[test]
    fn a_loop_does_not_jump_to_the_stop_over_the_road() {
        // out along y = 0 to x = 1000, back along y = 12: stop 1 at x = 100 going out, stop 5
        // at x = 100 coming back, 12 m apart (#254)
        let mut trip = planned(0.0, &[(0.0, 0.0, 0.0), (100.0, 60.0, 60.0), (500.0, 120.0, 120.0), (1000.0, 200.0, 200.0), (500.0, 280.0, 280.0), (100.0, 340.0, 340.0), (0.0, 400.0, 400.0)]);
        for (i, s) in trip.stops.iter_mut().enumerate() {
            if i >= 4 {
                s.position.as_mut().unwrap().y = 12.0;
            }
        }
        trip.set_dirs();
        let mut d = PlayerDuty {
            line: "5".into(),
            tour: "1".into(),
            trips: vec![trip],
            trip_index: 0,
            first_trip: 0,
            next_stop: 0,
            at_stop: false,
            arrived_late: None,
            done: false,
            served_terminus: None,
            left_late: None,
            held_back: false,
            placed: true,
            trip_changed: false,
            picked: true,
            first_update: None,
            heading: 90.0,
        };
        // at stop 0, then leaving east
        d.advance(glam::DVec3::new(0.0, 0.0, 0.0), 0.0);
        d.advance(glam::DVec3::new(60.0, 0.0, 0.0), 30.0);
        assert_eq!(d.next_stop, 1);
        // at stop 1 heading east: stop 5 (12 m away, the other way round) is not taken
        d.advance(glam::DVec3::new(100.0, 0.0, 0.0), 60.0);
        d.advance(glam::DVec3::new(140.0, 0.0, 0.0), 70.0);
        assert_eq!(d.next_stop, 2, "the duty goes on to stop 2, not over the road to stop 5");
    }

    #[test]
    fn a_duty_starts_with_the_trip_that_fits_the_time() {
        // Spandau line 5, tour "Mo-Fr 3": a depot run 14:44-15:01, then 15:07 and 16:01
        let trips = vec![
            planned(
                53040.0,
                &[(0.0, 53040.0, 53040.0), (500.0, 54060.0, 54060.0)],
            ),
            planned(
                54420.0,
                &[(500.0, 54420.0, 54420.0), (1000.0, 56400.0, 56400.0)],
            ),
            planned(
                57660.0,
                &[(1000.0, 57660.0, 57660.0), (500.0, 59400.0, 59400.0)],
            ),
        ];
        assert_eq!(
            starting_trip(&trips, 15.0 * 3600.0 + 300.0),
            1,
            "15:05: the 15:07"
        );
        assert_eq!(
            starting_trip(&trips, 14.0 * 3600.0 + 50.0 * 60.0),
            0,
            "14:50: the depot run under way"
        );
        assert_eq!(
            starting_trip(&trips, 15.0 * 3600.0 + 1800.0),
            1,
            "15:30: the 15:07 under way"
        );
        assert_eq!(
            starting_trip(&trips, 23.0 * 3600.0),
            2,
            "after the last: the last"
        );
        let now = 15.0 * 3600.0 + 300.0;
        let mut d = PlayerDuty {
            line: "5".into(),
            tour: "3".into(),
            trips,
            trip_index: 1,
            first_trip: 0,
            next_stop: 0,
            at_stop: false,
            arrived_late: None,
            done: false,
            served_terminus: None,
            left_late: None,
            held_back: false,
            placed: false,
            trip_changed: false,
            picked: false,
            first_update: None,
            heading: 0.0,
        };
        // 200 m from the first stop two minutes before the departure: early, next stop the first
        assert_eq!(d.advance(glam::DVec3::new(300.0, 0.0, 0.0), now), None);
        assert_eq!(d.next_stop, 0);
        assert!((d.delay(now) + 120.0).abs() < 1e-9);
        // at the first stop, leaving a minute late
        d.advance(glam::DVec3::new(500.0, 0.0, 0.0), now + 60.0);
        assert!(d.at_stop);
        assert_eq!(
            d.advance(glam::DVec3::new(560.0, 0.0, 0.0), 54480.0).map(|(_, left)| left),
            Some(60.0)
        );
        assert_eq!(d.next_stop, 1);
        // on the way the delay is what it left with until the next stop is overdue
        assert!((d.delay(55000.0) - 60.0).abs() < 1e-9);
        assert!((d.delay(56600.0) - 200.0).abs() < 1e-9);
        // the next trip does not take over while this one is driven ...
        d.advance(glam::DVec3::new(800.0, 0.0, 0.0), 57620.0);
        assert_eq!(d.trip_index, 1);
        // ... but once its end is reached
        d.advance(glam::DVec3::new(1000.0, 0.0, 0.0), 57630.0);
        assert!(d.done && !d.take_trip_change());
        d.advance(glam::DVec3::new(1000.0, 0.0, 0.0), 57640.0);
        assert_eq!((d.trip_index, d.next_stop), (2, 0));
        assert!(d.take_trip_change());
    }

    #[test]
    fn a_duty_starts_with_a_trip_the_bus_can_reach() {
        let trips = vec![
            planned(54420.0, &[(500.0, 54420.0, 54420.0), (1000.0, 56400.0, 56400.0)]),
            planned(57660.0, &[(1000.0, 57660.0, 57660.0), (500.0, 59400.0, 59400.0)]),
        ];
        let duty = |trips: Vec<PlannedTrip>| PlayerDuty {
            line: "5".into(),
            tour: "3".into(),
            trips,
            trip_index: 0,
            first_trip: 0,
            next_stop: 0,
            at_stop: false,
            arrived_late: None,
            done: false,
            served_terminus: None,
            left_late: None,
            held_back: false,
            placed: false,
            trip_changed: false,
            picked: false,
            first_update: None,
            heading: 0.0,
        };
        // 4 km away two minutes before the 15:07 leaves: the duty begins with the 16:01
        let mut d = duty(trips.clone());
        d.advance(glam::DVec3::new(-3500.0, 0.0, 0.0), 54300.0);
        assert_eq!((d.trip_index, d.next_stop), (1, 0));
        assert!(d.delay(54300.0) < -3000.0, "early for the 16:01");
        // under way already and nothing later: the last trip, from its first stop the bus
        // can still make on time
        let mut d = duty(trips[1..].to_vec());
        d.advance(glam::DVec3::new(-3500.0, 0.0, 0.0), 58000.0);
        assert_eq!((d.trip_index, d.next_stop), (0, 1));
        // ... or from its first when none can be
        let mut d = duty(trips[1..].to_vec());
        d.advance(glam::DVec3::new(-3500.0, 0.0, 0.0), 59000.0);
        assert_eq!((d.trip_index, d.next_stop), (0, 0));
    }

    #[test]
    fn ibis_skips_a_service_leg_for_the_player_display() {
        let mut service = planned(100.0, &[(0.0, 100.0, 100.0), (100.0, 200.0, 200.0)]);
        service.line.clear();
        service.terminus = "Betriebsfahrt".into();
        let mut passenger = planned(300.0, &[(100.0, 300.0, 300.0), (200.0, 400.0, 400.0)]);
        passenger.line = "5E".into();
        let d = PlayerDuty {
            line: "5E".into(),
            tour: "1".into(),
            trips: vec![service, passenger],
            trip_index: 0,
            first_trip: 0,
            next_stop: 1,
            at_stop: false,
            arrived_late: None,
            done: false,
            served_terminus: None,
            left_late: None,
            held_back: false,
            placed: false,
            trip_changed: false,
            picked: false,
            first_update: None,
            heading: 0.0,
        };
        let (trip, stop) = d.trip_for_ibis();
        assert_eq!(trip.line, "5E");
        assert_eq!(trip.terminus, "T");
        assert_eq!(stop, 0);
    }

    #[test]
    fn the_player_picks_the_trip_to_start_with() {
        let trips = vec![
            planned(4.0 * 3600.0 + 7.0 * 60.0, &[(0.0, 0.0, 0.0)]),
            planned(4.0 * 3600.0 + 22.0 * 60.0, &[(0.0, 0.0, 0.0)]),
            planned(4.0 * 3600.0 + 37.0 * 60.0, &[(0.0, 0.0, 0.0)]),
        ];
        assert_eq!(chosen_trip(&trips, "04:22"), Some(1));
        assert_eq!(chosen_trip(&trips, "4:30"), Some(2), "the next one leaving");
        assert_eq!(chosen_trip(&trips, "05:00"), None);
        assert_eq!(chosen_trip(&trips, "1"), Some(0));
        assert_eq!(chosen_trip(&trips, "3"), Some(2));
        assert_eq!(chosen_trip(&trips, "4"), None);
    }

    #[test]
    fn bays() {
        // the stop's box offset is kept as it is until the vehicle is known
        for lat in [0.0, 2.0, -4.0] {
            assert_eq!(bay_offset(lat), lat);
        }
    }
}
