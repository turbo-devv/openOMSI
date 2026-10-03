//! AI road traffic: vehicles from `ailists.cfg` moving on the map's path network, the
//! traffic light programs of the junctions, and the population of cars around the player.

/// Seconds a car held only by a full exit waits before it squeezes in (see `junction`).
/// Seconds a car waits at a junction's line before it keeps a claim on its way through
/// while waiting (see `Traffic::junction`).
const LONG_WAIT_CLAIM: f32 = 45.0;
const GRIDLOCK_WAIT: f32 = 45.0;
use crate::bus_service::{BusService, Phase};
use crate::scene::{VehicleRender, World};
use anyhow::Result;
use glam::{DVec2, DVec3};
use hashbrown::HashMap;
use omsi_render::{Renderer, Scene};
use omsi_sim::ai_motion::{
    back_in_ramp, pull_out_ramps, AiBody, MotionKind, BACK_IN_LAT_ACCEL, PULL_OUT_ACCEL,
    PULL_OUT_CLEARANCE,
};
use omsi_sim::collision::Obb;
use omsi_sim::traffic::{
    arrival_time, AiState, Aspect, LaneKind, Lead, Network, TrafficLightController, MAX_BRAKE,
};
use omsi_sim::vehicle::AiFrame;
use omsi_sim::{VehicleInstance, VehicleType};
use std::path::Path;
use std::sync::Arc;

/// How many of a type's paint schemes the AI uses: every scheme is a full upload of the
/// bus's textures the first time it appears, which used to cost a frame of 100-200 ms
/// each and a minute of stutter after loading Spandau.
pub const AI_SCHEMES: usize = 4;
const SCRIPT_UPLOAD_BUDGET: usize = 4 << 20;

/// Start a vehicle of type `ty` once and throw it away: its `{init}` and its displays read
/// the files they need (depot data, fonts) into the caches before the first real one of the
/// type comes along in the middle of a drive.
pub fn warm_up(world: &World, ty: &Arc<VehicleType>, hof: Option<Arc<omsi_vehicle::Hof>>) {
    let t = std::time::Instant::now();
    let mut host = omsi_sim::VehicleHost::new(omsi_sim::SimClock::default());
    host.hof = hof;
    host.font_lib = Some(world.fonts.clone());
    let mut vehicle = VehicleInstance::new(ty.clone(), host);
    vehicle.init_text_textures(&mut world.fonts.lock(), &|p| {
        omsi_texture::decode_file(p)
            .ok()
            .map(|i| (i.width, i.height, i.rgba))
    });
    if omsi_cfg::env::var_os("OMSI_PROFILE").is_some() {
        log::info!(
            "  first start of {}: {:.1} ms",
            ty.def.path.display(),
            t.elapsed().as_secs_f64() * 1000.0
        );
    }
}

/// Pulling out onto the other half of the road round something standing in the lane.
#[derive(Debug, Clone, Copy)]
pub struct Passing {
    /// The lane of the oncoming traffic the car moves over onto.
    pub lane: usize,
    /// How far to the left that lane lies (m).
    pub side: f32,
    /// Odometer reading at which the car is past the obstacle and moves back.
    pub until: f32,
    /// Odometer reading at which the car's front would reach the obstacle (had it stayed in
    /// its lane): until then it can still give up and stop behind it.
    pub block: f32,
    /// Length of the S-curve back into the lane (m).
    pub back: f32,
    /// Given up because somebody came the other way: back in and stopping at the odometer
    /// reading `hold`.
    pub aborted: bool,
    pub hold: f32,
    /// Started from a standstill close behind the obstacle: the car edges out
    /// (`PULL_OUT_ACCEL`) until its front is past the obstacle's corner.
    pub creep: bool,
}

impl Passing {
    /// Odometer reading at which the car has moved back far enough to be out of the way of
    /// the oncoming traffic (`half_width` its own half width).
    fn clear_at(&self, half_width: f32) -> f32 {
        self.until
            + self.back
                * omsi_sim::traffic::ramp_progress_for(self.side, half_width + ONCOMING_ROOM)
    }
}

/// Seconds a timetable bus waits where the route it has ends for the rest of its route
/// (tiles bring it as they load) before it gives the trip up (see `Traffic::tick`).
const ROUTE_WAIT_MAX: f32 = 40.0;

/// A car edging out round something standing keeps to `PULL_OUT_ACCEL` until its front is
/// this far past the obstacle's rear (m).
const CREEP_PAST: f32 = 2.0;
/// How far ahead an emergency vehicle warns what holds it up (`TrafficPriorityWarningNeeded`,
/// m).
const PRIORITY_WARN_GAP: f32 = 60.0;

/// Seconds a car at `st` needs to drive `dist` metres out on the other half of the road:
/// the first `creep` metres edging out, the rest speeding up to `v_cap` (a driver standing
/// still first reacts).
fn pass_time(dist: f32, creep: f32, st: &AiState, v_cap: f32) -> f32 {
    let wait = if st.speed < 0.1 { st.reaction } else { 0.0 };
    let accel = st.accel * 0.85;
    if creep <= 0.0 || dist <= 0.0 {
        return wait + arrival_time(dist, st.speed, accel, v_cap);
    }
    let a0 = accel.min(PULL_OUT_ACCEL);
    let first = creep.min(dist);
    let v1 = (st.speed * st.speed + 2.0 * a0 * first)
        .sqrt()
        .min(v_cap.max(st.speed));
    wait + arrival_time(first, st.speed, a0, v_cap) + arrival_time(dist - first, v1, accel, v_cap)
}

/// Room an oncoming vehicle needs beside a car (m from the car's side to the middle of the
/// oncoming lane): its half width and a margin.
const ONCOMING_ROOM: f32 = 1.15;
/// The player's box as `player_in_way` sees it is this much longer at each end (m).
const PLAYER_BOX_MARGIN: f32 = 0.5;
/// Somebody on foot more than this far above or below a car's way is not in it (m).
const PEOPLE_LEVEL: f64 = 2.5;
/// A car that gave up and has stood this long (s) held by nothing anybody can see goes even
/// in view (see `Traffic::populate_seen`).
const PHANTOM_WAIT: f32 = 90.0;
/// Another vehicle more than this far above or below a car's way (where the way passes it)
/// is not in it (m): the road under a bridge is some 4.5 m below the deck.
const BODY_LEVEL: f64 = 3.0;

/// What makes a new car a timetable bus (`Traffic::create_car`).
pub struct BusSetup {
    /// The trip's lanes, as far as the loaded tiles have them.
    pub route: Vec<usize>,
    pub stops: Vec<crate::bus_service::Stop>,
    /// Fleet number and registration (`number`, `ident` string variables).
    pub number: Option<(String, String)>,
    pub hof: Option<Arc<omsi_vehicle::Hof>>,
}

pub struct AiCar {
    /// Stable id for references from other systems (passengers).
    pub id: u64,
    /// The random seed it was made with and its paint scheme: a car that goes out of range
    /// and comes back is the same car (`DormantCar`).
    pub seed: u64,
    pub scheme: Option<usize>,
    pub state: AiState,
    pub vehicle: VehicleInstance,
    pub render: VehicleRender,
    pub trailer_renders: Vec<VehicleRender>,
    /// The body following the way `state` lays out.
    pub body: AiBody,
    /// Seconds this car has been standing still without a stop of its own: a red light or
    /// a queue is seconds, a jam that never clears grows without bound.
    pub stopped: f32,
    /// The car it follows now (its id), when one is close ahead.
    pub lead_car: Option<u64>,
    /// A car it does not take for its lead until the time given: two that had each other
    /// for their lead (see `Traffic::break_lead_pairs`).
    pub ignore_lead: Option<(u64, f64)>,
    /// Seconds it has crept along below 1 m/s (a claim of one that crawls in a jam of its
    /// own is no car about to come either).
    pub crawl: f32,
    /// The odometer when it last got two metres further, and the seconds since: a car
    /// that creeps against something it never gets past stands as much as one that stops
    /// (`stopped` starts afresh with every centimetre it creeps).
    pub progress: (f32, f32),
    /// A timetable bus: its trip's stops, the doors, the layover, the people aboard (see
    /// `bus_service`). Everything else about it is this car's.
    pub bus: Option<Box<BusService>>,
    /// `[sound_ai]` set, created when the car comes near the listener.
    pub sounds: Option<omsi_audio::SoundSet>,
    /// Half the vehicle's width (m).
    pub half_width: f32,
    /// Waiting at a junction for someone with the right of way this frame.
    pub yielding: bool,
    /// Stopped by a red light this frame.
    pub light_hold: bool,
    /// Junction lanes this car has claimed to drive through (`TPathInfo::reservePaths`).
    pub reserved: Vec<usize>,
    /// The light (controller, lamp) the driver decided to pass on yellow.
    pub amber: Option<(usize, usize)>,
    pub passing: Option<Passing>,
    /// Finished (a dead end, the end of a timetable trip, given up): taken off the road as
    /// soon as nobody can see it.
    pub gone: bool,
    /// Seconds since it was put on the road are fewer than this: its speed was a guess.
    pub fresh: f32,
    /// The car it lets go first at the next merge (by id).
    pub merge_after: Option<u64>,
    /// What holds it (`OMSI_DEBUG_TRAFFIC`, for cars standing for long).
    pub holding: Option<String>,
    /// What held the car back this frame (for OMSI_TRACE_AI): the constraint nearest ahead
    /// ("lead", "light", "yield", "merge", "keep_back", "people", "pull_out", "service",
    /// "end", "" for none) and its distance ahead of the front (m).
    pub why: (&'static str, f32),
    /// Something made it wait this frame: a stop point, or a car or an obstacle close ahead.
    pub held: bool,
    /// The vehicle (by id) whose body stands in this car's way off its lanes this frame
    /// (`Traffic::body_in_way`).
    pub geo_block: Option<u64>,
    /// What it keeps behind (by id) and the gap to it, as of its last step.
    pub lead_info: Option<(u64, f32)>,
    /// What it waited for at its last junction (`OMSI_DEBUG_STUCK` only).
    pub junction_why: String,
    /// Giving way: where it waits (distance from its origin to the line).
    pub wait_at: Option<f32>,
    /// The vehicle (by id) standing half out of the lane that this car is squeezing past.
    pub squeeze: Option<u64>,
    /// How far behind something standing (a bus at its stop, the player's bus) this car
    /// stops, so that it can steer out round it later (m, front bumper to the other's body;
    /// from its own steering, `pull_out_room`).
    pub pass_room: f32,
    /// No new look at passing before this time (a pull-out that did not clear the corner is
    /// not tried again every frame).
    pub pass_retry: f32,
    /// The traffic light it waited for in the last frame: distance from its origin.
    pub light_at: Option<f32>,
    /// A car that was parked at the kerb: seconds it still stands there, indicating, before
    /// it pulls out (see `Traffic::pull_out_parked`).
    pub pull_out: f32,
    /// Parking: the free space it drives into (see `Traffic::park_in`).
    pub park: Option<ParkPlan>,
    /// A rail vehicle: the track it has come along, (odometer, point), oldest first -
    /// where its rear bogie and its coupled cars and sections run (see `rail_behind`).
    pub rail_trail: std::collections::VecDeque<(f64, DVec3)>,
    /// A train turned round as a whole (its last car leads now): what a trip's
    /// `[trainreverse]` is compared with (Omsi.exe's vehicle +0x4e1).
    pub consist_reversed: bool,
}

/// A free parking space beside a lane that a car means to park in: the space of parked car
/// `key` that drove off (its object comes back when the car is in).
#[derive(Debug, Clone, Copy)]
pub struct ParkPlan {
    pub key: i64,
    pub lane: usize,
    /// The space's middle along the lane and its offset to the right of it (m).
    pub s: f32,
    pub lat: f32,
    /// Moving over into the space.
    pub ramped: bool,
    /// In the space and standing: the parked object takes its place at the next sync.
    pub done: bool,
}

impl AiCar {
    /// A timetable bus (in service or on its way off after its trip).
    pub fn is_bus(&self) -> bool {
        self.bus.is_some()
    }

    /// Bound to rails (a train, a tram).
    pub fn is_rail(&self) -> bool {
        self.body.kind == MotionKind::Rail
    }

    /// Boarding at a stop: the script is told to open the doors (`AI_Scheduled_AtStation`).
    pub fn at_station(&self) -> bool {
        self.bus.as_ref().map(|b| b.at_station()).unwrap_or(false)
    }

    /// The side's doors to open at the stop it is boarding at (`AI_Scheduled_AtStation_Side`).
    pub fn at_station_side(&self) -> f32 {
        self.bus.as_ref().map(|b| b.at_station_side()).unwrap_or(0.0)
    }

    /// Standing at one of its stops (doors open, waiting for the departure, pulling out).
    pub fn at_stop(&self) -> bool {
        self.bus.as_ref().map(|b| b.at_stop()).unwrap_or(false)
    }

    pub fn trip_done(&self) -> bool {
        self.bus.as_ref().map(|b| b.trip_done()).unwrap_or(false)
    }

    pub fn route_open(&self) -> bool {
        self.bus.as_ref().map(|b| b.route_open).unwrap_or(false)
    }

    /// The next stop: (route index, distance along that lane).
    pub fn next_stop(&self) -> Option<(usize, f32)> {
        self.bus.as_ref()?.stops.front().map(|s| (s.ri, s.s))
    }

    /// Seconds it will still stand at its stop.
    pub fn standing_for(&self, day_time: f64) -> f32 {
        self.bus.as_ref().map(|b| b.standing_for(day_time)).unwrap_or(0.0)
    }
}

/// Where a vehicle's body stands, for the checks that go by geometry rather than by lanes:
/// the car it belongs to (a trailer or rear section counts as its own footprint), centre,
/// forward and right unit vectors, half length, half width and speed along its heading.
#[derive(Debug, Clone, Copy)]
struct Footprint {
    car: usize,
    center: DVec2,
    fwd: DVec2,
    right: DVec2,
    half_len: f64,
    half_w: f64,
    speed: f32,
    /// Height of the vehicle's origin (an aircraft overhead is not in a car's way).
    z: f64,
}

/// A car's own footprint, grown forward by `ahead` metres.
fn car_foot(c: &AiCar, ahead: f32) -> Footprint {
    let st = &c.state;
    let h = c.vehicle.heading.to_radians();
    let (fwd, right) = (DVec2::new(h.sin(), h.cos()), DVec2::new(h.cos(), -h.sin()));
    let center = c.vehicle.position.truncate() + fwd * ((st.front + ahead - st.rear) * 0.5) as f64;
    Footprint { car: usize::MAX, center, fwd, right, half_len: ((st.front + ahead + st.rear) * 0.5) as f64, half_w: c.half_width as f64, speed: st.speed, z: c.vehicle.position.z }
}

impl Footprint {
    fn from_obb(car: usize, b: &omsi_sim::collision::Obb, speed: f32) -> Footprint {
        let (sh, ch) = (b.heading.sin(), b.heading.cos());
        Footprint {
            car,
            center: b.center,
            fwd: DVec2::new(sh, ch),
            right: DVec2::new(ch, -sh),
            half_len: b.half.y,
            half_w: b.half.x,
            speed,
            z: b.z0,
        }
    }

    /// The footprint as a collision box (no height range).
    fn obb(&self) -> Obb {
        Obb {
            center: self.center,
            half: DVec2::new(self.half_w, self.half_len),
            heading: self.fwd.x.atan2(self.fwd.y),
            z0: f64::MIN,
            z1: f64::MAX,
            velocity: DVec2::ZERO,
            mass: 0.0,
            pole: None,
            id: -1,
        }
    }

    /// Does this footprint overlap `o`, both grown by `margin` (separating axes)?
    fn overlaps(&self, o: &Footprint, margin: f64) -> bool {
        let d = o.center - self.center;
        for axis in [self.fwd, self.right, o.fwd, o.right] {
            let extent = |f: &Footprint| {
                (f.fwd.dot(axis)).abs() * (f.half_len + margin)
                    + (f.right.dot(axis)).abs() * (f.half_w + margin)
            };
            if d.dot(axis).abs() > extent(self) + extent(o) {
                return false;
            }
        }
        true
    }
}

/// The player's vehicle as the traffic sees it: centre, heading (deg), half length, half
/// width, speed along the heading (m/s, negative when reversing).
pub type PlayerBox = (DVec3, f64, f32, f32, f32);

/// Where the player looks from, for putting cars on the road and taking them off only
/// where nobody sees it happen.
#[derive(Debug, Clone, Copy)]
pub struct Viewer {
    pub pos: DVec3,
    pub forward: DVec3,
    /// Tangents of half the horizontal and vertical field of view.
    pub tan_x: f64,
    pub tan_y: f64,
    /// Beyond this distance nothing shows (fog, or a car smaller than a pixel) (m).
    pub range: f64,
    /// What the renderer leaves out (see `omsi_render::RenderOptions`): objects smaller on
    /// the screen than `min_size` (the original's measure), and farther than `max_dist`
    /// (0 = no limit); `fov` is the vertical field of view (radians).
    pub min_size: f64,
    pub max_dist: f64,
    pub fov: f64,
}

/// A car further away than this is below two pixels on a 900-line screen (m).
const VISIBLE_RANGE: f64 = 900.0;

/// Within this distance (m) of the camera a timetable bus has its driver at the wheel.
const DRIVER_NEAR: f64 = 70.0;

/// Within this distance of the camera no car appears or vanishes, seen or not: the mirrors
/// and a turn of the head see what is near (see [`Traffic::may_appear`]).
const NEVER_VANISH_WITHIN: f64 = 150.0;
/// Within this distance a vehicle appears or vanishes only behind something, wherever the
/// player looks (see `Traffic::hidden`).
const NEAR_HIDE: f64 = 350.0;

/// Within this distance of the camera an AI vehicle is animated and drawn even out of the
/// view (m): the mirrors look behind, and a car beside the view throws its shadow into it.
const UNSEEN_NEAR: f64 = 80.0;

/// How near an articulated AI bus has to be for its bellows to be reshaped as its joint
/// turns (m): the fold of the bend is a few centimetres, which is a screen pixel or more
/// within this range - farther out it is not worth a mesh update every frame it steers.
const SKIN_DISTANCE: f64 = 200.0;

impl Viewer {
    pub fn new(cam: &omsi_render::Camera, aspect: f64, fog_range: f64) -> Viewer {
        let tan_y = (cam.fov_deg as f64 * 0.5).to_radians().tan();
        Viewer {
            pos: cam.position,
            forward: cam.forward().as_dvec3().normalize_or_zero(),
            tan_x: tan_y * aspect.max(0.2),
            tan_y,
            range: fog_range.min(VISIBLE_RANGE).min(cam.far as f64),
            min_size: 0.0,
            max_dist: 0.0,
            fov: (cam.fov_deg as f64).to_radians(),
        }
    }

    /// A wider picture than the camera's own (a triple screen's side panels): the tangents
    /// of its half-angles, horizontal and vertical. The size limit stays the camera's.
    pub fn with_extent(mut self, extent: Option<(f64, f64)>) -> Viewer {
        if let Some((tan_x, tan_y)) = extent {
            self.tan_x = self.tan_x.max(tan_x);
            self.tan_y = self.tan_y.max(tan_y);
        }
        self
    }

    /// The renderer's culling as well (`RenderOptions::min_obj_size`, `max_obj_dist`).
    pub fn with_culling(mut self, min_size: f32, max_dist: f32) -> Viewer {
        self.min_size = min_size.max(0.0) as f64;
        self.max_dist = max_dist.max(0.0) as f64;
        self
    }

    /// Would the renderer draw an object of radius `r` this far away at all? Beyond that a
    /// car can come and go in plain view without anybody seeing it happen.
    pub fn draws(&self, dist: f64, r: f64) -> bool {
        // (the renderer measures a vehicle by a sphere about its origin, which may stand
        // well off its middle: half as much again, and a metre, to be sure)
        let r = r * 1.5 + 1.0;
        if self.max_dist > 0.0 && dist > self.max_dist + r {
            return false;
        }
        self.min_size <= 0.0 || 2.0 * r / (dist.max(0.01) * self.fov.max(1e-3)) >= self.min_size
    }

    /// Does a sphere of radius `r` at `p` lie within the view frustum and range?
    pub fn frames(&self, p: DVec3, r: f64) -> bool {
        let rel = p - self.pos;
        let dist = rel.length();
        if dist <= r {
            return true;
        }
        if dist - r > self.range {
            return false;
        }
        let f = self.forward;
        let mut right = f.cross(DVec3::Z);
        if right.length() < 1e-3 {
            right = DVec3::X;
        }
        let right = right.normalize();
        let up = right.cross(f);
        let z = rel.dot(f);
        if z < -r {
            return false;
        }
        let (x, y) = (rel.dot(right), rel.dot(up));
        x.abs() <= z * self.tan_x + r * (1.0 + self.tan_x * self.tan_x).sqrt()
            && y.abs() <= z * self.tan_y + r * (1.0 + self.tan_y * self.tan_y).sqrt()
    }
}

/// How far from the player cars are kept (m) and how far out of sight one may be before it
/// is taken off (m); in plain view a car stays until it is too small to see.
const DESPAWN_FACTOR: f64 = 1.6;

/// A random car out of the player's range: still on the map and still driving, but without
/// a body, a script or a picture - a few numbers. It comes back as the same car (type,
/// paint, id) where it has got to when the player comes near, and it only ever leaves the
/// map at the end of the road network. Before, every car out of range was simply taken
/// away and new ones made up around the player: the traffic followed the player about, and
/// a car driven past was never seen again.
pub struct DormantCar {
    pub id: u64,
    pub ty: Arc<VehicleType>,
    pub kind: LaneKind,
    pub lane: usize,
    pub s: f32,
    pub speed: f32,
    pub seed: u64,
    pub scheme: Option<usize>,
    /// Its own dice for the turns it takes.
    pub walk: u64,
}

/// How many cars a whole map keeps at most, as a multiple of the number asked for around the
/// player (memory: a dormant car is a few dozen bytes, but each one woken is a full vehicle).
const MAP_POPULATION_FACTOR: f32 = 8.0;


fn street_lane_weight(l: &omsi_sim::traffic::Lane) -> Option<f64> {
    (l.kind == LaneKind::Street && !l.no_cars && l.density > 0.0 && l.length() >= 8.0)
        .then(|| l.length() as f64 * l.density.clamp(0.05, 4.0) as f64)
}

pub struct Traffic {
    pub net: Network,
    /// Sum of the spawn weights of every street lane, updated only as tiles add lanes.
    street_weight: f64,
    /// Parked cars standing in or beside a lane: per lane, (distance along it, signed
    /// lateral offset of the car's centre, + = right). A car in the lane's middle is an
    /// obstacle to stop behind; one over the kerb side is passed with a swerve to the left.
    parked: HashMap<usize, Vec<(f32, f32)>>,
    /// Parked cars no lane has been found beside yet: the lane may come with a tile that is
    /// not loaded yet (a road spline often starts in the next tile). Most stand in car parks
    /// and stay here.
    parked_waiting: Vec<DVec3>,
    /// Tiles whose lanes the network has (lanes stay once they are in).
    pub lane_tiles: hashbrown::HashSet<(i32, i32)>,
    /// Counts the times tiles brought their lanes: whoever resolved something against the
    /// network and missed a part of it looks again when this changes.
    pub lanes_generation: u64,
    /// AI vehicle types with weight, the lane kind they run on (`[type]` 2 rail, 3 air)
    /// and their group in `groups`.
    types: Vec<(Arc<VehicleType>, f32, LaneKind, usize)>,
    /// The random traffic groups: name, `unsched_trafficdens.txt` factor and day curves.
    groups: Vec<omsi_map::ailists::UnschedGroup>,
    /// The map has an `unsched_trafficdens.txt` (else the global.cfg curve applies).
    group_curves: bool,
    /// Each group's place in `unsched_vehgroups.txt`, the number a path's `[rule]
    /// trafficdensity` names it by (None: the map has no such file, and every group drives
    /// wherever the lane's density lets traffic).
    group_uvg: Vec<Option<usize>>,
    /// The default density of every `unsched_vehgroups.txt` entry, in file order: 0 none, 1
    /// for the first entry its medium density, for any other that of the first entry, 2 of
    /// the second, and so on. It applies on the paths without a rule for the group.
    uvg_defaults: Arc<Vec<i32>>,
    pub cars: Vec<AiCar>,
    /// The random cars out of range (see `DormantCar`).
    pub dormant: Vec<DormantCar>,
    /// `time` when the dormant cars last moved on.
    dormant_time: f32,
    rng: u64,
    /// Target number of cars around the camera.
    pub target: usize,
    /// Made only so that the light programs run (no traffic, no timetable): nobody is put
    /// on the roads - no aircraft, no parked car pulling out - while `target` is 0.
    pub lights_only: bool,
    pub spawn_radius: f64,
    pub time: f32,
    /// Renders of cars that have gone, given back at the next `sync`.
    released: Vec<VehicleRender>,
    /// Where the camera is (the window sets it before `sync`): far cars show their script
    /// textures as stand-ins.
    pub camera: Option<DVec3>,
    /// Sound sets of despawned cars, stopped at the next audio update.
    orphan_sounds: Vec<omsi_audio::SoundSet>,
    lights: Vec<TrafficLightController>,
    controller_of_object: HashMap<i64, usize>,
    /// Coupled vehicle types by file.
    trailer_types: HashMap<std::path::PathBuf, Option<Arc<VehicleType>>>,
    /// `[sound_ai]` configurations by file.
    sound_cfgs: HashMap<std::path::PathBuf, Option<Arc<omsi_vehicle::SoundCfg>>>,
    root: std::path::PathBuf,
    /// Car-frames spent waiting for a red light (statistics).
    pub held_at_red: usize,
    /// Who wants a timetable bus to stop (`Humans::stop_wishes`): the buses somebody
    /// aboard wants to get off, the stops where somebody waits. None without passengers:
    /// every bus then serves every stop.
    stop_wishes: Option<(hashbrown::HashSet<u64>, hashbrown::HashSet<i64>)>,
    /// Seconds the player's vehicle has been standing.
    player_still: f32,
    /// Time of day (seconds since midnight); light cycles and timetables run on it.
    pub day_time: f64,
    /// How fast the clock runs (the time speed): the timetable keeps to it.
    pub time_scale: f64,
    /// Day of the week (0 Monday … 6 Sunday) for the traffic density curves.
    pub weekday: i32,
    /// Street lights on → AI vehicles switch their lights on.
    pub night: bool,
    /// The light of the day, for the cars' `Envir_Brightness` (see `sync`).
    pub daylight: Option<omsi_sim::Daylight>,
    next_id: u64,
    /// The last car that started an overtake and when (for chase-camera debugging).
    pub last_overtaker: Option<(u64, f32)>,
    /// The first car that entered a turning lane and when (`--follow turn`).
    pub first_turner: Option<(u64, f32)>,
    /// The first car that stopped at a red light (`--follow red`), the first that gave way
    /// at a junction (`--follow yield`), the first that pulled out onto the other side of
    /// the road round an obstacle or squeezed past a bus at its stop (`--follow pass`).
    pub first_red: Option<(u64, f32)>,
    pub first_yield: Option<(u64, f32)>,
    pub first_passer: Option<(u64, f32)>,
    /// `[trafficdensity_road]` curve of the map: (hour, factor).
    pub density_curve: Vec<(f32, f32)>,
    /// The options' `[AIUnschedFactor]`: the share of the random traffic.
    pub unsched_factor: f32,
    /// The options' `[AIMaxCountScheduled]` (0 = no limit).
    pub max_scheduled: u32,
    /// Where the player looks from (set every frame).
    pub viewer: Option<Viewer>,
    /// Buildings that hide what is behind them (the player's collision world).
    pub occluders: Option<Arc<omsi_sim::collision::CollisionWorld>>,
    /// Pedestrians on the footpaths: (lane, distance along it), for giving way at crossings
    /// and for the pedestrian lights' request buttons.
    pub walkers: Vec<(usize, f32)>,
    /// Everybody on foot on the ground: position, velocity and whether they are waiting
    /// at a stop (set every frame) - the cars stop for anybody in their way, not only on
    /// a crossing.
    pub people: Vec<(DVec3, DVec2, bool)>,
    /// No car has been placed yet: the first population may fill the view.
    initial: bool,
    /// Seconds of the last tick (the lamp scripts run in `sync`).
    last_dt: f32,
    /// Game time since the lamps' scripts last ran (see `sync`).
    lamp_dt: f32,
    /// `OMSI_TRACE_AI=<file.csv>`: every car's pose, steering and speed, every frame.
    trace: Option<std::io::BufWriter<std::fs::File>>,
    /// Cars already reported for a hard bend (`OMSI_DEBUG_TRAFFIC`).
    logged_hard: hashbrown::HashSet<u64>,
    /// `OMSI_DEBUG_LIGHTS=all|near|<controller>,…`: which programs log their changes, and
    /// the states they showed last.
    light_log: Option<String>,
    light_prev: Vec<Vec<i32>>,
    /// `OMSI_DEBUG_POPULATION`: log where cars appear and vanish relative to the view.
    debug_population: bool,
    /// Cars placed since the last look inside the view frustum (hidden behind something):
    /// (id, position). `OMSI_POPULATION_SHOTS` photographs them to check.
    pub framed_spawns: Vec<(u64, DVec3)>,
    /// The player's vehicle as of the last tick (nothing is put on the road on top of it).
    player: Option<PlayerBox>,
    /// The player's bus has right of way over the traffic (its script's `TrafficPriority`,
    /// OMSI: priority 1000 over the types' own): cars keep out of the way it is about
    /// to take for longer.
    pub player_priority: bool,
    /// The player's indicators (0 off, 1 left, 2 right, 3 hazard; `lan::indicator`), set
    /// before each `tick`.
    pub player_blinker: u8,
    /// Seconds since the player's bus last showed the indicator towards the traffic (the
    /// lamps go dark half of the time).
    player_signal_age: f32,
    /// ... and for how long it has been indicating so (s).
    player_signalling: f32,
    /// The player's vehicle and the LAN players' as the junctions see them (`way_user_on`),
    /// as of this tick.
    way_users: Vec<WayUser>,
    /// The LAN players' vehicles (their session ids and boxes as for the player), set
    /// before each `tick`: the cars stop behind them and go round them as round the
    /// player's bus.
    pub others: Vec<(u32, PlayerBox)>,
    /// The drivers at the wheel of the timetable buses near the camera, by car id (see
    /// `driver.rs`; made within `DRIVER_NEAR` m of the camera, let go beyond twice that).
    drivers: HashMap<u64, crate::driver::DriverFigure>,
    /// Figures let go by their bus, hidden, for the next one (their GPU meshes stay).
    driver_pool: Vec<crate::driver::DriverFigure>,
    /// Where the last `tick` spent its time (s, OMSI_PROFILE): who is on which lane and the
    /// light programs, every car's plan, the bodies and scripts on the workers.
    pub tick_split: [f64; 3],
    /// Seconds each of them has stood still.
    others_still: HashMap<u32, f32>,
    /// Per car: `AiCar::geo_block` of the frame before (who waits for whom by geometry).
    geo_prev: Vec<Option<u64>>,
    /// Car index by id (as of the start of the tick).
    index_of: HashMap<u64, usize>,
    /// `pull_out_room` by vehicle file.
    pull_out_rooms: HashMap<std::path::PathBuf, f32>,
    /// Timetable buses taken off the road because the tile under them was unloaded (their
    /// ids), for the timetable to put them back when the tiles come again.
    pub removed_scheduled: Vec<u64>,
    /// One-way lanes that have had their reverse twin added (`add_reverse_twins`).
    twinned: hashbrown::HashSet<usize>,
    /// Bodies besides the AI vehicles' that no timetable vehicle may be put into: the
    /// player's vehicle and the LAN players' (set before each `Schedule::tick`).
    pub keep_clear: Vec<omsi_sim::collision::Obb>,
    /// LAN play: this game draws the host's traffic instead of its own (`lan_world`).
    mirror: bool,
    /// Count only the cars within this distance of this point when filling up (the
    /// population around a LAN player, `populate_lan_centers`).
    count_near: Option<(DVec3, f64)>,
    /// LAN play: where the other players are (host): the traffic is kept around them too.
    pub lan_centers: Vec<DVec3>,
}

/// `[boundingbox]` of a vehicle that gives none.
const DEFAULT_BOX: [f32; 6] = [2.5, 12.0, 3.0, 0.0, 0.0, 1.5];

/// The bodies of a vehicle and of the parts coupled to it, where they stand.
pub fn vehicle_bodies(v: &VehicleInstance) -> Vec<omsi_sim::collision::Obb> {
    let mut out = vec![omsi_sim::collision::Obb::from_box(
        v.ty.def.bounding_box.unwrap_or(DEFAULT_BOX),
        v.position,
        v.heading,
    )];
    for t in &v.trailers {
        let heading = if t.reversed {
            t.heading + 180.0
        } else {
            t.heading
        };
        out.push(omsi_sim::collision::Obb::from_box(
            t.ty.def.bounding_box.unwrap_or(DEFAULT_BOX),
            t.position,
            heading,
        ));
    }
    out
}

/// Cruising speed of an AI aircraft where its flight path sets no limit (km/h): an
/// airliner on its final approach.
const AIRCRAFT_KMH: f32 = 280.0;
/// How far ahead a car looks for other vehicles at least (m).
const LOOK_AHEAD: f32 = 70.0;
/// ... and at most, when it is fast.
const LOOK_AHEAD_MAX: f32 = 150.0;

/// How far ahead a driver at `speed` watches for something standing in the way: far enough
/// to slow down gently for it. With a fixed 70 m a car at 50-65 km/h first saw the player's
/// bus standing (or a bus at its stop) so late that the following model braked at 4-5 m/s².
/// A timetable bus's IBIS moves on to its next stop as the driver would press it on: the
/// stock scripts' interior displays, announcements and side displays read `IBIS_busstop`
/// (an index into the depot file's stop list of the route), which nothing moved on an AI
/// bus - its saloon display stood on the first stop for the whole trip. `remaining` is the
/// number of stops still to come.
pub(crate) fn ibis_to_next_stop(v: &mut VehicleInstance, remaining: usize) {
    let Some(ri) = v.var("IBIS_RouteIndex").filter(|r| *r >= 0.0) else { return };
    let Some(n) = v.host.hof.as_ref().and_then(|h| h.info_busstop_lists.get(ri as usize)).map(|l| l.len()) else { return };
    if n == 0 || v.var("IBIS_busstop").is_none() {
        return;
    }
    let idx = n.saturating_sub(remaining.max(1)).min(n - 1);
    v.set_var("IBIS_busstop", idx as f32);
}

fn look_ahead(speed: f32) -> f32 {
    (speed * speed / 3.0 + speed * 2.0 + 20.0).clamp(LOOK_AHEAD, LOOK_AHEAD_MAX)
}

/// Ground height for an AI vehicle's wheels: the road surface (blended between raster
/// texels), else any surface, else the terrain.
fn ai_ground(world: &World) -> Arc<dyn Fn(f64, f64) -> Option<f64> + Send + Sync> {
    let terrains = world.terrains.clone();
    let surfaces = world.surfaces.clone();
    Arc::new(move |x, y| {
        let tx = (x / omsi_map::tile_size()).floor() as i32;
        let ty = (y / omsi_map::tile_size()).floor() as i32;
        let lx = (x - tx as f64 * omsi_map::tile_size()) as f32;
        let ly = (y - ty as f64 * omsi_map::tile_size()) as f32;
        let surface = surfaces.read().get(&(tx, ty)).cloned();
        if let Some(h) = surface.and_then(|s| s.sample_road_smooth(lx, ly)) {
            return Some(h as f64);
        }
        let t = terrains.read();
        Some(t.get(&(tx, ty))?.sample(lx, ly) as f64)
    })
}

/// The bare ground at a point (no roads, decks or platforms on it).
fn terrain_height(world: &World, x: f64, y: f64) -> Option<f64> {
    let tx = (x / omsi_map::tile_size()).floor() as i32;
    let ty = (y / omsi_map::tile_size()).floor() as i32;
    let t = world.terrains.read();
    let terrain = t.get(&(tx, ty))?;
    Some(terrain.sample(
        (x - tx as f64 * omsi_map::tile_size()) as f32,
        (y - ty as f64 * omsi_map::tile_size()) as f32,
    ) as f64)
}

/// How a vehicle on lanes of `kind` moves.
fn motion_kind(kind: LaneKind) -> MotionKind {
    match kind {
        LaneKind::Air => MotionKind::Air,
        LaneKind::Rail => MotionKind::Rail,
        _ => MotionKind::Road,
    }
}

/// How far ahead of the player's bus centre a car looks for it (m): the bus's half length,
/// and where it will be in `horizon` seconds for a car whose way crosses the bus's. Not
/// for one going the same way (`way_dir` within 60 degrees of the bus's heading): with the
/// bus behind it, that stretch ahead of the bus reached over the car itself and it braked
/// for a bus that was only following it (#139).
fn player_reach_ahead(half_len: f32, speed: f32, horizon: f32, fwd: DVec2, way_dir: DVec2) -> f64 {
    let same_way = way_dir.length() > 0.5 && way_dir.normalize().dot(fwd) > 0.5;
    half_len as f64 + if same_way { 0.0 } else { (speed.max(0.0) * horizon) as f64 }
}

/// How much track an AI rail vehicle keeps behind it (m): a long train's length.
const RAIL_TRAIL: f64 = 400.0;

/// Note where an AI rail vehicle is: `odometer` (m) and the point of its way there. A jump
/// (put somewhere else, turned round at a terminus) starts the trail afresh.
fn record_rail_trail(trail: &mut std::collections::VecDeque<(f64, DVec3)>, odometer: f64, here: DVec3) {
    if let Some(&(u, p)) = trail.back() {
        if (here - p).truncate().length() > (odometer - u).abs() + 2.0 {
            trail.clear();
        } else if (odometer - u).abs() <= 0.5 {
            return;
        }
    }
    // (backing up takes the trail back with it)
    while trail.back().is_some_and(|b| b.0 > odometer) {
        trail.pop_back();
    }
    trail.push_back((odometer, here));
    while trail.front().is_some_and(|f| odometer - f.0 > RAIL_TRAIL) {
        trail.pop_front();
    }
}

/// The point of an AI rail vehicle's track `d` metres behind its origin: on the trail it
/// came along. (Its way knows only the lane it came off; farther back it runs straight on,
/// and a train's last cars stood beside the track after a pair of points.) Where the trail
/// does not reach - the last half metre, a vehicle just put there - the way.
fn rail_behind(trail: &std::collections::VecDeque<(f64, DVec3)>, state: &AiState, net: &Network, d: f64) -> DVec3 {
    let u = state.odometer as f64 - d;
    let newest = trail.back().map_or(f64::MIN, |b| b.0);
    if u >= newest {
        return state.way_point(net, -d as f32);
    }
    crate::rail_drive::point_at(trail, u).unwrap_or_else(|| state.way_point(net, -d as f32))
}

/// A body for a vehicle that has just been put on the way `state` describes, with the
/// vehicle posed on it.
fn place_body(
    net: &Network,
    state: &AiState,
    vehicle: &mut VehicleInstance,
    kind: MotionKind,
) -> AiBody {
    let mut body = AiBody::new(&vehicle.ty.def, kind);
    let ground = vehicle.ground.clone();
    let contact = vehicle.contact.clone();
    body.place(
        &|d| state.way_point(net, d),
        ground
            .as_ref()
            .map(|g| g.as_ref() as &dyn Fn(f64, f64) -> Option<f64>),
        contact.as_deref(),
        state.speed,
    );
    body.apply(vehicle);
    body
}

/// The `OMSI_TRACE_AI` file, with its header written.
fn open_trace() -> Option<std::io::BufWriter<std::fs::File>> {
    use std::io::Write;
    let path = omsi_cfg::env::var_os("OMSI_TRACE_AI")?;
    let mut f = std::io::BufWriter::new(
        std::fs::File::create(&path)
            .map_err(|e| log::warn!("OMSI_TRACE_AI: {e}"))
            .ok()?,
    );
    writeln!(f, "t,id,type,x,y,z,heading,pitch,bank,steer,speed,lane,s,blinker,turn,lane_heading,lateral,at_station,acc,yielding,light_hold,passing,front,rear,half_width,scheduled,why,why_gap,phase,lane_z").ok()?;
    Some(f)
}

/// The vehicle's extent from its origin: (to the front bumper, to the rear bumper, half
/// the width) from its `[boundingbox]`.
fn extents(ty: &VehicleType, length: f32) -> (f32, f32, f32) {
    match ty.def.bounding_box {
        Some(bb) if bb[1] > 1.0 => (
            bb[1] * 0.5 + bb[4],
            bb[1] * 0.5 - bb[4],
            (bb[0] * 0.5).max(0.5),
        ),
        // without a `[boundingbox]` the model's own box, as Omsi.exe takes it (0x7b5da4):
        // the Berlin S-Bahn's cars, 18 m long, counted as 12 m ones
        _ => match ty.model_box() {
            Some((lo, hi)) if hi.y - lo.y > 1.0 => (hi.y.max(0.5), (-lo.y).max(0.5), (hi.x.max(-lo.x)).max(0.5)),
            _ => (length * 0.5, length * 0.5, 0.9),
        },
    }
}

/// The driver of a random car: how fast, how close, how patient (see `AiState`).
fn personality(state: &mut AiState, seed: u64, heavy: bool) {
    let r = |k: u32| ((seed >> k) & 0xff) as f32 / 255.0;
    state.desire = if heavy {
        0.88 + 0.1 * r(3)
    } else {
        0.9 + 0.22 * r(3)
    };
    state.headway = 1.0 + 0.8 * r(11);
    state.min_gap = 1.6 + 1.4 * r(19);
    state.accel = if heavy {
        0.8 + 0.4 * r(27)
    } else {
        1.3 + 1.0 * r(27)
    };
    state.decel = if heavy { 1.6 } else { 2.0 + 0.8 * r(35) };
    state.accept_gap = 3.0 + 2.5 * r(43);
    state.reaction = 0.4 + 0.8 * r(51);
}

/// Where a vehicle meets a crossing lane on its way: its lane in the sequence, the distance
/// from its origin to that lane's start.
#[derive(Debug, Clone)]
struct Junction {
    /// (lane, distance from the car's origin to its start) of the junction's lanes on the
    /// car's way; the first is where it has to wait.
    lanes: Vec<(usize, f32)>,
    /// The lane after the junction and the distance to its start.
    exit: Option<(usize, f32)>,
    /// The car is already on one of the junction's lanes.
    inside: bool,
}

/// A vehicle the AI does not drive (the player's bus, a LAN player's) as the right of way
/// sees it: Omsi.exe keeps the player's vehicle on the paths like any AI vehicle, so the
/// cars give way to it by the same rules; here it is put onto the lanes where it is.
#[derive(Debug, Clone)]
struct WayUser {
    /// The lane it is on and those it may take next: (lane, distance from its centre to
    /// the lane's start - negative for the lane it is on).
    lanes: Vec<(usize, f32)>,
    /// Speed along its way (m/s), half its length (m), how long it has stood (s).
    speed: f32,
    half_len: f32,
    still: f32,
    /// Its script claims priority (`TrafficPriority`).
    prio: bool,
}

/// Seconds until a vehicle `dist` metres from a point gets its front there, from speed `v`
/// with acceleration `a`.
fn time_to(dist: f32, v: f32, a: f32) -> f32 {
    if dist <= 0.0 {
        return 0.0;
    }
    let a = a.max(0.3);
    // v t + a t² / 2 = dist
    (-v + (v * v + 2.0 * a * dist).sqrt()) / a
}

fn crossing_arrival(st: &AiState, distance: f32, claimed: bool, waits_short: bool, stalled: bool) -> f32 {
    if distance <= 0.3 {
        return 0.0;
    }
    if claimed {
        return time_to(distance, st.speed, st.accel)
            + if st.speed < 0.1 { st.reaction } else { 0.0 };
    }
    if waits_short {
        return f32::MAX;
    }
    // A queue cannot accelerate freely. Keep its actual movement in the prediction:
    // ignoring a crawling car altogether would let another drive into its path.
    if stalled {
        return if st.speed > 0.0 { distance / st.speed } else { f32::MAX };
    }
    if st.speed > 0.5 {
        distance / st.speed
    } else {
        time_to(distance, 0.0, st.accel) + st.reaction
    }
}

/// A vehicle the AI does not drive, put onto the lanes for the right of way: the street
/// lane it drives along (within 3.5 m, its heading within 45 degrees) and the lanes it
/// may take on from there, as far as it gets in about six seconds. Where the way forks,
/// an indicator picks the turns that way (when the fork has one); without one every
/// branch counts - the cars cannot know where the bus is going, and the autopilot's
/// left turn without an indicator ran into a car that had taken the bus for going
/// straight on. None off the lanes or reversing.
fn way_user_on(net: &Network, b: &PlayerBox, blinker: u8, still: f32, prio: bool) -> Option<WayUser> {
    let (pos, heading, half_len, _, speed) = *b;
    if speed < -0.3 {
        return None;
    }
    let (lane, s, _) = net.lane_along(pos, heading, LaneKind::Street, 3.5, 45.0)?;
    let reach = (speed.max(0.0) * 6.0 + 40.0).min(LOOK_AHEAD_MAX);
    let mut lanes = vec![(lane, -s)];
    let mut open = vec![(lane, net.lanes[lane].length() - s)];
    while let Some((l, to_start)) = open.pop() {
        if to_start > reach || lanes.len() > 48 {
            continue;
        }
        let next: Vec<usize> = net.lanes[l]
            .next
            .iter()
            .copied()
            .filter(|&n| net.lanes[n].kind == LaneKind::Street && !lanes.iter().any(|o| o.0 == n))
            .collect();
        let chosen: Vec<usize> = match blinker {
            1 | 2 if next.len() > 1 && next.iter().any(|&n| net.lanes[n].turn == blinker as i32) => {
                next.into_iter().filter(|&n| net.lanes[n].turn == blinker as i32).collect()
            }
            _ => next,
        };
        for n in chosen {
            lanes.push((n, to_start));
            open.push((n, to_start + net.lanes[n].length()));
        }
    }
    Some(WayUser { lanes, speed: speed.max(0.0), half_len, still, prio })
}

/// The player's bus (or a LAN player's) coming to the joint where the car's (`me`) lane runs
/// into the one the bus is on - a side road's right turn into the main road, two lanes
/// becoming one - as `obstacle_ahead` sorts out two AI cars there: whoever gets to the
/// joint first goes first (a near tie to the bus), and the car keeps behind the bus as
/// if it were ahead in its own lane. Before, the car only saw the bus once its box was in
/// the car's way, and pulled out in front of it.
fn merging_lead(net: &Network, me: &AiState, users: &[WayUser]) -> Option<Lead> {
    if users.is_empty() || me.change.is_some() {
        return None;
    }
    let mut best: Option<Lead> = None;
    let mut before = net.lanes[me.lane].length() - me.s;
    let mut from = me.lane;
    for next in me.upcoming().take(2) {
        if before > LOOK_AHEAD {
            break;
        }
        if before - me.front < 0.5 {
            // at the joint already
            before += net.lanes[next].length();
            from = next;
            continue;
        }
        for &f in net.prev.get(next).map(|v| v.as_slice()).unwrap_or(&[]) {
            if f == from || net.crossings[from].iter().any(|c| c.other == f && c.merge) {
                continue;
            }
            for u in users {
                let (Some(&(_, df)), true) = (u.lanes.iter().find(|x| x.0 == f), u.lanes.iter().any(|x| x.0 == next)) else { continue };
                // its centre to the joint
                let to_joint = df + net.lanes[f].length();
                let theirs = to_joint - u.half_len;
                if to_joint < -u.half_len || (u.speed < 0.5 && u.still > 2.0) {
                    continue; // past it (then it is ahead in the lane), or standing
                }
                let t_me = (before - me.front).max(0.0) / me.speed.max(1.0);
                let t_them = if u.speed > 0.5 { theirs.max(0.0) / u.speed } else { time_to(theirs, 0.0, 1.0) + 1.0 };
                if t_them > t_me + 0.4 {
                    continue;
                }
                // behind it at the joint; while it is not past, wait at the joint itself
                let d = before - to_joint - u.half_len;
                let l = if d >= 0.0 {
                    Lead { gap: d - me.front, speed: u.speed, acc: 0.0 }
                } else {
                    Lead { gap: (before - 1.0 - me.front).max(0.0), speed: 0.0, acc: 0.0 }
                };
                if best.map(|b| l.gap < b.gap).unwrap_or(true) {
                    best = Some(l);
                }
            }
        }
        before += net.lanes[next].length();
        from = next;
    }
    best
}

/// What a vehicle the AI does not drive means for a car at a meeting place of its junction.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Verdict {
    /// Nothing: through already, far off, standing, or the car goes first.
    Free,
    /// It is in the meeting place, or in the junction and there first: wait (a car that
    /// cannot stop any more still goes).
    Hard,
    /// It has the right of way and comes within the gap the driver would take.
    Ruled,
}

/// `Traffic::junction_stop` for one vehicle the AI does not drive (`u`, its lane's start
/// `dm` m from its centre) at the meeting place `c` of the car's junction lane, `point` m
/// from the car's origin: the rules the cars keep among themselves. `inside`: the car is
/// in the junction already; `committed`: it has claimed it; `me_prio`: it has priority by
/// its script; `must_yield`: its lane gives way to the vehicle's; `patience` shrinks the
/// gap it wants after a long wait. Also when the vehicle arrives (s) and when the car
/// would be through (s).
#[allow(clippy::too_many_arguments)]
fn way_user_verdict(st: &AiState, u: &WayUser, dm: f32, c: &omsi_sim::traffic::Crossing, point: f32, inside: bool, committed: bool, me_prio: bool, must_yield: bool, patience: f32) -> (Verdict, f32, f32) {
    let (v, a_me) = (st.speed, st.accel);
    // (its centre to the meeting point, and its front to the meeting place)
    let dj = dm + c.other_at;
    let t_clear = time_to(point + c.after + st.rear + 0.3, v, a_me) + if v < 0.5 { st.reaction } else { 0.0 };
    if dj + c.other_after < -u.half_len - 0.3 {
        return (Verdict::Free, f32::MAX, t_clear); // through
    }
    // (a meeting place far into the junction is weighed once the car is in it)
    if !inside && point - c.before - st.front > 25.0 {
        return (Verdict::Free, f32::MAX, t_clear);
    }
    let theirs = dj - c.other_before - u.half_len;
    let t_mine = time_to(point - c.before - st.front, v, a_me) + if v < 0.1 { st.reaction } else { 0.0 };
    if dm <= 0.0 && theirs <= 0.3 {
        // on that lane and in the meeting place: unless this car is further in already
        // (only where it is: a bus at its stop line is not on any of the ways it may take
        // yet, and counted as in all of them it held the cross traffic it waited for)
        let mine_in = st.front - (point - c.before);
        let ahead = mine_in > 0.0 && mine_in > -theirs + 0.3;
        return (if ahead { Verdict::Free } else { Verdict::Hard }, 0.0, t_clear);
    }
    // standing (at a stop, at its own line, letting this car go): not coming
    if u.speed < 0.5 && u.still > 2.0 {
        return (Verdict::Free, f32::MAX, t_clear);
    }
    let t_j = if u.speed > 0.5 { theirs.max(0.0) / u.speed } else { time_to(theirs, u.speed, 1.0) + 1.0 };
    if dm <= 0.0 && u.speed > 0.5 {
        // in the junction already and moving: the first there goes first
        let first = !(committed || inside) || t_j < t_mine - 0.3;
        let verdict = if first && t_j < t_clear + 1.0 { Verdict::Hard } else { Verdict::Free };
        return (verdict, t_j, t_clear);
    }
    if inside || committed || (me_prio && !u.prio) || (!must_yield && !(u.prio && !me_prio)) {
        return (Verdict::Free, t_j, t_clear);
    }
    // the gap a driver takes in the main road's traffic (as for the cars)
    let verdict = if t_j < (st.accept_gap + 2.0).max(t_clear + 1.0) * patience { Verdict::Ruled } else { Verdict::Free };
    (verdict, t_j, t_clear)
}

impl Traffic {
    /// Make the population deterministic for a LAN room.  The room's session id is
    /// shared by the host and every client, so the same map/time produces the same
    /// initial cars instead of each process inventing a different world.
    pub fn set_lan_seed(&mut self, seed: u64) {
        self.rng = seed | 1;
    }

    /// Build the network from the lanes collected by `World::build_scene` and load the AI
    /// car types of the map's `ailists.cfg` (the `[aigroup_2]` groups that are not depots).
    pub fn new(root: &Path, world: &World, target: usize) -> Result<Traffic> {
        let (lanes, parked_cars, lane_tiles) = take_from_tiles(world);
        let mut net = Network {
            lanes,
            // (`[lht]`: priority to the left, passing on the right, keeping left)
            left_hand: world.global.left_hand_traffic,
            ..Default::default()
        };
        if net.left_hand {
            log::info!("traffic: the map drives on the left");
        }
        net.link(1.5);
        let mut types = Vec::new();
        let mut groups: Vec<omsi_map::ailists::UnschedGroup> = Vec::new();
        let mut group_uvg: Vec<Option<usize>> = Vec::new();
        let mut uvg_defaults: Vec<i32> = Vec::new();
        // `unsched_trafficdens.txt`: per random group a factor and its density over the day
        // (by day of the week); the global.cfg curve is the fallback of maps without it
        let dens: Vec<omsi_map::ailists::UnschedGroup> =
            omsi_cfg::CfgFile::read(&world.map_dir.join("unsched_trafficdens.txt"))
                .ok()
                .map(|f| omsi_map::ailists::parse_unsched_trafficdens(&f))
                .unwrap_or_default();
        let group_curves = !dens.is_empty();
        {
            // `unsched_vehgroups.txt` names the groups the random traffic is made of. The
            // other `[aigroup_2]`s exist only for the timetable: on Berlin-Spandau "Pan Am"
            // and "Mi-8 Soviet AF" fly TXL.ttl and Relais.ttl, and taking them into the
            // random pool put airliners on the flight paths at any hour of the day. Its
            // number is the group's default density on the paths without a `[rule]
            // trafficdensity` for it (see `uvg_density`): 0 means only where the paths ask
            // for the group. Taken as "off", Spandau had no trucks and no Trabant at all,
            // though 865 paths ask for the one and 462 around Falkensee for the other.
            // `OMSI_TRAFFIC_ALL_GROUPS=1` lets such groups drive everywhere (and, on a map
            // without the file, every group, not only the default one).
            let all_groups = omsi_cfg::env::var_os("OMSI_TRAFFIC_ALL_GROUPS").is_some();
            let unscheduled: Option<Vec<(String, i32)>> =
                omsi_cfg::CfgFile::read(&world.map_dir.join("unsched_vehgroups.txt"))
                    .ok()
                    .map(|f| {
                        omsi_map::ailists::parse_unsched_vehgroups(&f)
                            .into_iter()
                            .map(|(n, c)| (n.trim().to_ascii_lowercase(), c))
                            .collect()
                    });
            if let Some(names) = &unscheduled {
                log::info!("random traffic groups (unsched_vehgroups.txt): {names:?}");
                uvg_defaults = names
                    .iter()
                    .map(|n| if all_groups && n.1 <= 0 { 1 } else { n.1 })
                    .collect();
            }
            let lists = &world.ailists;
            // Without `unsched_vehgroups.txt` the random traffic is the ailists' default group
            // alone (the first, or the one the `[ailist]` header names): Omsi.exe 0x785f98
            // makes one nameless group then, and a nameless group takes the default group.
            // Taking every group instead, a map whose ailists keep an ambulance (or a bus,
            // or a lorry) in a group of its own had one car in four of that kind (#1025).
            if unscheduled.is_none() && !all_groups {
                if let Some(g) = lists.groups.get(lists.default_group) {
                    log::info!("random traffic: no unsched_vehgroups.txt, only the default AI group {}", g.name);
                }
            }
            for (_, g) in lists.groups.iter().enumerate().filter(|(i, g)| {
                !g.is_depot
                    && g.hof.is_none()
                    && (unscheduled.is_some() || all_groups || *i == lists.default_group)
                    && !g
                        .vehicles
                        .iter()
                        .any(|v| v.file.to_ascii_lowercase().ends_with(".zug"))
            }) {
                let lname = g.name.trim().to_ascii_lowercase();
                let uvg = match &unscheduled {
                    Some(names) => match names.iter().position(|n| n.0 == lname) {
                        None => continue,
                        Some(u) => {
                            if uvg_defaults.get(u).copied().unwrap_or(0) <= 0 {
                                log::info!(
                                    "random traffic group {} drives only where its paths ask for it (unsched_vehgroups.txt)",
                                    g.name
                                );
                            }
                            Some(u)
                        }
                    },
                    None => None,
                };
                let gi = groups.len();
                group_uvg.push(uvg);
                groups.push(
                    dens.iter()
                        .find(|d| d.name.trim().eq_ignore_ascii_case(g.name.trim()))
                        .cloned()
                        .unwrap_or(omsi_map::ailists::UnschedGroup {
                            name: g.name.clone(),
                            factor: if group_curves { 0.0 } else { 1.0 },
                            densities: Vec::new(),
                        }),
                );
                for v in &g.vehicles {
                    let lower = v.file.to_ascii_lowercase();
                    if lower.ends_with(".zug")
                        || lower.contains("trains\\")
                        || lower.contains("trains/")
                    {
                        continue;
                    }
                    let path = omsi_cfg::resolve_path(root, &v.file);
                    match VehicleType::load_ai(root, &path) {
                        Ok(t) => {
                            // rail (only as scheduled trains), 3 = aircraft on flight paths
                            let rail = t.def.is_rail();
                            let air =
                                matches!(t.def.kind, omsi_vehicle::vehicle::VehicleKind::Other(3));
                            if rail {
                                log::debug!(
                                    "AI vehicle {} is rail-bound, not street traffic",
                                    v.file
                                );
                            } else {
                                types.push((
                                    Arc::new(t),
                                    v.weight.max(0.0),
                                    if air { LaneKind::Air } else { LaneKind::Street },
                                    gi,
                                ));
                            }
                        }
                        Err(e) => log::warn!("AI vehicle {}: {e}", v.file),
                    }
                }
            }
        }
        // No buses in the random road traffic: the depot groups of `ailists.cfg` are the
        // fleet the *timetable* drives, and OMSI puts a bus on a street only because a trip
        // of the map's TTData runs there. Mixing the depot fleet into the random pool put
        // the map's one bus type on every road of the map - on Grundorf that is a single
        // articulated GN92, which is why it seemed to be a type of our own choosing.
        if let Ok(list) = omsi_cfg::env::var("OMSI_DEBUG_LANES") {
            // lane indices, or `at:x,y,r` for the street lanes passing within r m of a point
            // (the indices change from run to run on a map whose tiles load in parallel)
            let chosen: Vec<usize> = match list.strip_prefix("at:") {
                Some(rest) => {
                    let v: Vec<f64> = rest
                        .split(',')
                        .filter_map(|x| x.trim().parse().ok())
                        .collect();
                    let (p, r) = (
                        DVec3::new(
                            v.first().copied().unwrap_or(0.0),
                            v.get(1).copied().unwrap_or(0.0),
                            0.0,
                        ),
                        v.get(2).copied().unwrap_or(10.0),
                    );
                    (0..net.lanes.len())
                        .filter(|&i| {
                            net.lanes[i].kind == LaneKind::Street
                                && net.lanes[i]
                                    .points
                                    .iter()
                                    .any(|q| (q.truncate() - p.truncate()).length() < r)
                        })
                        .collect()
                }
                None => list
                    .split(',')
                    .filter_map(|v| v.trim().parse::<usize>().ok())
                    .filter(|&i| i < net.lanes.len())
                    .collect(),
            };
            for i in chosen {
                let l = &net.lanes[i];
                let samples: Vec<String> = (0..l.points.len())
                    .step_by((l.points.len() / 8).max(1))
                    .map(|k| {
                        format!(
                            "[{:.1} m h {:.1} k {:.3}]",
                            l.dist[k],
                            l.headings[k],
                            l.curvature.get(k).copied().unwrap_or(0.0)
                        )
                    })
                    .collect();
                log::info!("lane {i}: {} {:?} rev {} turn {} prio {} len {:.1} start ({:.1}, {:.1}) end ({:.1}, {:.1}) next {:?} light {:?} crossings {:?} {}", l.name, l.key, l.reversed, l.turn, l.priority, l.length(), l.start().x, l.start().y, l.end().x, l.end().y, l.next, l.traffic_light, net.crossings.get(i), samples.join(" "));
            }
        }
        if omsi_cfg::env::var_os("OMSI_DEBUG_WHEELS").is_some() {
            for (t, ..) in &types {
                let v = VehicleInstance::new(
                    t.clone(),
                    omsi_sim::VehicleHost::new(omsi_sim::SimClock::default()),
                );
                for line in v.wheel_pivot_report() {
                    log::info!("wheel pivot: {line}");
                }
            }
        }
        let lights = world.traffic_lights.lock().clone();
        let controller_of_object = world.controller_of_object.lock().clone();
        let parked: HashMap<usize, Vec<(f32, f32)>> = HashMap::new();
        let turning = net.lanes.iter().filter(|l| l.turn != 0).count();
        let with_side = net
            .lanes
            .iter()
            .filter(|l| l.left.is_some() || l.right.is_some())
            .count();
        let turn_lanes = net
            .lanes
            .iter()
            .filter(|l| {
                (l.left.is_some() || l.right.is_some())
                    && l.next.iter().any(|&n| net.lanes[n].turn != 0)
            })
            .count();
        let closed = net
            .lanes
            .iter()
            .filter(|l| l.no_cars || l.density <= 0.0)
            .count();
        let closed_junctions = net
            .lanes
            .iter()
            .filter(|l| (l.no_cars || l.density <= 0.0) && l.source == 2)
            .count();
        let quiet = net
            .lanes
            .iter()
            .filter(|l| l.density < 1.0 && l.density > 0.0)
            .count();
        let prio = net
            .lanes
            .iter()
            .filter(|l| (l.priority - omsi_sim::traffic::DEFAULT_PRIORITY).abs() > 0.5)
            .count();
        log::info!("traffic: {} lanes ({turning} turning, {with_side} with a neighbour, {turn_lanes} where a turn lane applies, {closed} closed to cars of which {closed_junctions} are junctions, {quiet} with less traffic by [rule], {prio} with a [rule] priority), {} AI vehicle types in {} groups, {} light programs, {} lamps", net.lanes.len(), types.len(), groups.len(), lights.len(), world.light_objects.lock().len());
        // lights on paths a car reaches from a lit path of the same crossing (they hold
        // only a car that comes into the crossing there, see `light_at_entry`)
        let inner_lights = (0..net.lanes.len())
            .filter(|&l| {
                let lane = &net.lanes[l];
                lane.traffic_light.is_some()
                    && lane.source == 2
                    && net.prev.get(l).is_some_and(|ps| {
                        ps.iter().any(|&p| {
                            let q = &net.lanes[p];
                            q.source == 2 && q.traffic_light.is_some() && q.key.map(|k| (k.tile, k.id)) == lane.key.map(|k| (k.tile, k.id))
                        })
                    })
            })
            .count();
        log::info!("traffic: {inner_lights} lit paths inside crossings (a car already in the crossing is not held there again)");
        let light_log = omsi_cfg::env::var("OMSI_DEBUG_LIGHTS").ok();
        let light_prev = lights.iter().map(|c| vec![-100; c.lights.len()]).collect();
        let lanes = 0..net.lanes.len();
        let street_weight = net.lanes.iter().filter_map(street_lane_weight).sum();
        let mut t = Traffic {
            net,
            street_weight,
            parked,
            parked_waiting: Vec::new(),
            lane_tiles: lane_tiles.into_iter().collect(),
            lanes_generation: 0,
            types,
            groups,
            group_curves,
            group_uvg,
            uvg_defaults: Arc::new(uvg_defaults),
            cars: Vec::new(),
            dormant: Vec::new(),
            dormant_time: 0.0,
            rng: 0x9E37_79B9_7F4A_7C15,
            target,
            lights_only: false,
            spawn_radius: 400.0,
            time: 0.0,
            released: Vec::new(),
            camera: None,
            orphan_sounds: Vec::new(),
            lights,
            controller_of_object,
            trailer_types: HashMap::new(),
            sound_cfgs: HashMap::new(),
            root: root.to_path_buf(),
            held_at_red: 0,
            stop_wishes: None,
            player_still: 0.0,
            day_time: 0.0,
            time_scale: 1.0,
            weekday: 0,
            night: false,
            daylight: None,
            next_id: 1,
            last_overtaker: None,
            first_turner: None,
            first_red: None,
            first_yield: None,
            first_passer: None,
            density_curve: world.global.traffic_density_road.clone(),
            unsched_factor: crate::settings::Settings::load().ai_unsched_factor,
            max_scheduled: crate::settings::Settings::load().ai_max_scheduled,
            viewer: None,
            occluders: None,
            walkers: Vec::new(),
            people: Vec::new(),
            initial: true,
            last_dt: 0.0,
            lamp_dt: 0.0,
            trace: open_trace(),
            logged_hard: Default::default(),
            light_log,
            light_prev,
            debug_population: omsi_cfg::env::var_os("OMSI_DEBUG_POPULATION").is_some(),
            framed_spawns: Vec::new(),
            player: None,
            player_priority: false,
            player_blinker: 0,
            player_signal_age: f32::MAX,
            player_signalling: 0.0,
            way_users: Vec::new(),
            others: Vec::new(),
            drivers: HashMap::new(),
            driver_pool: Vec::new(),
            tick_split: [0.0; 3],
            others_still: HashMap::new(),
            geo_prev: Vec::new(),
            index_of: HashMap::new(),
            pull_out_rooms: HashMap::new(),
            removed_scheduled: Vec::new(),
            twinned: Default::default(),
            keep_clear: Vec::new(),
            mirror: false,
            count_near: None,
            lan_centers: Vec::new(),
        };
        t.sort_parked(parked_cars, lanes);
        Ok(t)
    }

    /// Attach explicitly listed cars (a `.zug` train): (type, reversed).
    pub fn attach_cars(
        &mut self,
        world: &World,
        renderer: &Renderer,
        scene: &mut Scene,
        car: usize,
        cars: &[(Arc<VehicleType>, bool)],
    ) {
        let c = &mut self.cars[car];
        for (t, rev) in cars {
            c.trailer_renders
                .push(world.add_vehicle_shared(renderer, scene, t, None, Some(&c.render)));
            c.vehicle.attach_trailer_ex(t.clone(), *rev);
        }
    }

    /// The cars of car `ci`'s train, front to back, each with whether it is turned round
    /// (the first, the one that drives, is not).
    pub(crate) fn consist(&self, ci: usize) -> Vec<(Arc<VehicleType>, bool)> {
        let v = &self.cars[ci].vehicle;
        std::iter::once((v.ty.clone(), false)).chain(v.trailers.iter().map(|t| (t.ty.clone(), t.reversed))).collect()
    }

    /// Couple `cars` behind car `ci` instead of the ones it has (a train made up anew).
    pub(crate) fn set_trailers(&mut self, world: &World, renderer: &Renderer, scene: &mut Scene, ci: usize, cars: &[(Arc<VehicleType>, bool)]) {
        let c = &mut self.cars[ci];
        for r in c.trailer_renders.drain(..) {
            world.release_vehicle(renderer, scene, r);
        }
        c.vehicle.trailers.clear();
        self.attach_cars(world, renderer, scene, ci, cars);
    }

    /// Turn train `ci` round as Omsi.exe does for a trip whose `[trainreverse]` differs from
    /// how the train stands (0x613a98): the whole consist the other way, its last car
    /// leading - here the vehicle that drives is made anew as that car, standing where it
    /// stands (at `s` on `lane`, which runs the new way), and the others coupled behind it
    /// in the opposite order, each turned round. `behind`: the lanes the train has behind
    /// it now, nearest last, for the track its cars stand on. The car keeps its id and its
    /// service.
    pub(crate) fn turn_train(&mut self, world: &World, renderer: &Renderer, scene: &mut Scene, ci: usize, lane: usize, s: f32, behind: &[usize], reversed: bool) {
        let cars: Vec<(Arc<VehicleType>, bool)> = self.consist(ci).into_iter().rev().map(|(t, r)| (t, !r)).collect();
        let Some((lead, lead_turned)) = cars.first().cloned() else { return };
        if lead_turned {
            // (a lead car turned round is drawn facing the way: none of the stock trains has one)
            log::debug!("train {}: its last car leads turned round", self.cars[ci].id);
        }
        let (id, seed, scheme) = (self.cars[ci].id, self.cars[ci].seed, self.cars[ci].scheme);
        // (where its cars stood, front to back: they stand there still, the other way round)
        let before: Vec<DVec3> = std::iter::once(self.cars[ci].vehicle.position).chain(self.cars[ci].vehicle.trailers.iter().map(|t| t.position)).collect();
        let center = self.viewer.map(|v| v.pos).unwrap_or_default();
        let kind = self.net.lanes[lane].kind;
        self.create_car(world, renderer, scene, center, kind, lane, s, lead, seed, Some(scheme), Some(id), Some(0.0), None);
        let Some(mut new) = self.cars.pop() else { return };
        let old = &mut self.cars[ci];
        // its service goes with it (its line and destination are set for the trip it takes
        // on); the way it drives, from where it stands
        new.vehicle.host.hof = old.vehicle.host.hof.clone();
        new.bus = old.bus.take();
        new.state.max_speed_kmh = old.state.max_speed_kmh;
        new.state.length = old.state.length;
        new.state.accel = old.state.accel;
        new.state.decel = old.state.decel;
        new.state.lat_accel = old.state.lat_accel;
        new.state.min_gap = old.state.min_gap;
        new.state.speed = 0.0;
        new.consist_reversed = reversed;
        let old = std::mem::replace(&mut self.cars[ci], new);
        self.orphan_sounds.extend(old.sounds);
        for r in std::iter::once(old.render).chain(old.trailer_renders) {
            world.release_vehicle(renderer, scene, r);
        }
        self.set_trailers(world, renderer, scene, ci, &cars[1..]);
        self.seed_rail_trail(ci, behind);
        let c = &mut self.cars[ci];
        let trail = &c.rail_trail;
        let (state, net) = (&c.state, &self.net);
        c.vehicle.retrail(0.0, &|d| Some(rail_behind(trail, state, net, d)));
        let after: Vec<DVec3> = std::iter::once(c.vehicle.position).chain(c.vehicle.trailers.iter().map(|t| t.position)).collect();
        let moved = before.iter().rev().zip(&after).map(|(a, b)| (*a - *b).truncate().length()).fold(0.0f64, f64::max);
        log::info!(
            "train {id} turned round: {} (its cars {:.1} m at most from where they stood)",
            std::iter::once(&c.vehicle.ty).chain(c.vehicle.trailers.iter().map(|t| &t.ty)).map(|t| t.def.path.file_stem().unwrap_or_default().to_string_lossy().to_string()).collect::<Vec<_>>().join(" + "),
            moved
        );
    }

    /// The track behind rail car `ci` as it has just been put on its lane: back along its
    /// lane and then `behind` (the lanes before it, nearest last), for its coupled cars.
    fn seed_rail_trail(&mut self, ci: usize, behind: &[usize]) {
        let c = &mut self.cars[ci];
        let net = &self.net;
        let odo = c.state.odometer as f64;
        let mut pts: Vec<(f64, DVec3)> = Vec::new();
        let (mut lane, mut s) = (c.state.lane, c.state.s);
        let mut back = behind.iter().rev();
        let mut d = 0.0f64;
        while d <= RAIL_TRAIL {
            pts.push((odo - d, net.lanes[lane].at(s.max(0.0)).0));
            s -= 1.0;
            if s < 0.0 {
                match back.next() {
                    Some(&l) => {
                        s += net.lanes[l].length();
                        lane = l;
                    }
                    None => break,
                }
            }
            d += 1.0;
        }
        c.rail_trail = pts.into_iter().rev().collect();
    }

    /// The vehicles coupled behind `ty` (its rear sections, trailers, the cars of a unit),
    /// each with whether it is turned round, loaded (once per file). As Omsi.exe builds a
    /// consist (0x70a174): towards the back of the train a vehicle goes on with its
    /// `[couple_back]`, or with its `[couple_front]` when it is itself turned round; the
    /// coupled one is turned round when the coupling's flag says so, against the one it
    /// hangs on; and a coupling back to the file it came from that turns nothing round is
    /// not followed. (Following `[couple_back]` whatever the way, the Berlin A3's unit -
    /// the S car and its K car turned round behind it, whose own `[couple_back]` names the
    /// S car again - went on S, K, S, K, S, none of them turned.)
    pub(crate) fn trailer_chain(&mut self, ty: &Arc<VehicleType>) -> Vec<(Arc<VehicleType>, bool)> {
        self.coupled_chain(ty, false, true)
    }

    /// See [`Traffic::trailer_chain`]: from `ty` (turned round: `rev`) towards the back of
    /// the train, or towards its front, the nearest first.
    pub(crate) fn coupled_chain(&mut self, ty: &Arc<VehicleType>, rev: bool, toward_back: bool) -> Vec<(Arc<VehicleType>, bool)> {
        let mut out = Vec::new();
        let (mut lead, mut lead_rev) = (ty.clone(), rev);
        for _ in 0..8 {
            let Some((path, r)) = crate::spawn::next_coupled(&lead.def, lead_rev, toward_back) else {
                break;
            };
            let root = self.root.clone();
            let t =
                self.trailer_types.entry(path.clone()).or_insert_with(
                    || match VehicleType::load_ai(&root, &path) {
                        Ok(t) => Some(Arc::new(t)),
                        Err(e) => {
                            log::warn!("trailer {}: {e}", path.display());
                            None
                        }
                    },
                );
            let Some(t) = t.clone() else { break };
            out.push((t.clone(), r));
            lead = t;
            lead_rev = r;
        }
        out
    }

    /// Load and attach the `[couple_back]` chain of `vehicle`; returns the renders.
    fn attach_trailers(
        &mut self,
        world: &World,
        renderer: &Renderer,
        scene: &mut Scene,
        vehicle: &mut VehicleInstance,
        scheme: Option<usize>,
        lead: &VehicleRender,
    ) -> Vec<VehicleRender> {
        let mut renders = Vec::new();
        let ty = vehicle.ty.clone();
        for (t, rev) in self.trailer_chain(&ty) {
            renders.push(world.add_vehicle_shared(
                renderer,
                scene,
                &t,
                scheme.filter(|i| *i < t.paint_schemes.len()),
                Some(lead),
            ));
            vehicle.attach_trailer_ex(t, rev);
        }
        renders
    }

    pub fn prime_pull_out_room(&mut self, ty: &VehicleType, bus: bool) {
        let (front, rear, half_width) = extents(ty, if bus { 12.0 } else { 4.5 });
        self.pull_out_room(ty, front, rear, half_width);
    }

    /// How far behind something standing a vehicle of `ty` stops so that it can pull out
    /// round it later (`omsi_sim::ai_motion::pull_out_room` against a standing bus with the
    /// oncoming lane 3.3 m over; by vehicle file).
    fn pull_out_room(&mut self, ty: &VehicleType, front: f32, rear: f32, half_width: f32) -> f32 {
        if let Some(&r) = self.pull_out_rooms.get(&ty.def.path) {
            return r;
        }
        // (a bus 2.5 m wide that stands up to 0.35 m further over than the car: bus and car
        // are rarely both in the middle of the lane)
        let r = omsi_sim::ai_motion::pull_out_room(&ty.def, (front, rear, half_width), 1.6, 3.3);
        if omsi_cfg::env::var_os("OMSI_DEBUG_TRAFFIC").is_some() {
            log::info!(
                "pull-out room of {}: {r:.2} m (front {front:.2}, half width {half_width:.2})",
                ty.def
                    .path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            );
        }
        self.pull_out_rooms.insert(ty.def.path.clone(), r);
        r
    }

    fn rand(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn rand_f(&mut self) -> f64 {
        (self.rand() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// How much traffic group `g` makes now: its `unsched_trafficdens.txt` factor times its
    /// curve for this day of the week (+1 Monday to Friday, +2 Saturday, +4 Sunday, 0 every
    /// day).
    fn group_density(&self, g: usize) -> f32 {
        let Some(gr) = self.groups.get(g) else {
            return 0.0;
        };
        if !self.group_curves {
            return 1.0;
        }
        let bit = match self.weekday {
            0..=4 => 1,
            5 => 2,
            _ => 4,
        };
        let hour = (self.day_time.rem_euclid(86400.0) / 3600.0) as f32;
        match gr
            .densities
            .iter()
            .find(|(mask, _)| *mask == 0 || mask & bit != 0)
        {
            Some((_, curve)) => gr.factor * omsi_map::global::curve_at(curve, hour).max(0.0),
            None => 0.0,
        }
    }

    /// The street traffic density now, 1 = the map's normal level, times the options' share
    /// of random traffic.
    fn street_density(&self) -> f32 {
        self.street_density_map() * self.unsched_factor
    }

    fn street_density_map(&self) -> f32 {
        if !self.group_curves {
            return omsi_map::global::curve_at(
                &self.density_curve,
                (self.day_time.rem_euclid(86400.0) / 3600.0) as f32,
            )
            .clamp(0.0, 2.0);
        }
        let groups: Vec<usize> = (0..self.groups.len())
            .filter(|&g| {
                self.types
                    .iter()
                    .any(|t| t.3 == g && t.2 == LaneKind::Street)
            })
            .collect();
        let factors: f32 = groups.iter().map(|&g| self.groups[g].factor).sum();
        if factors <= 0.0 {
            return 0.0;
        }
        (groups.iter().map(|&g| self.group_density(g)).sum::<f32>() / factors).clamp(0.0, 2.0)
    }

    /// How much of group `g`'s traffic `lane` carries: the path's `[rule] trafficdensity`
    /// for the group, else the group's default (see `uvg_defaults`).
    fn lane_group_density(&self, lane: &omsi_sim::traffic::Lane, g: usize) -> f32 {
        match self.group_uvg.get(g).copied().flatten() {
            Some(u) => lane.pool_density(&self.uvg_defaults, u),
            None => lane.density,
        }
    }

    /// A random vehicle type for a lane of `kind`: on a street `lane`, of the groups that
    /// lane carries, as much as it carries of each.
    fn pick_type(&mut self, kind: LaneKind, lane: Option<usize>) -> Option<Arc<VehicleType>> {
        // a vehicle's share: its weight within its group times what the group makes now
        let group_weight: Vec<f32> = (0..self.groups.len())
            .map(|g| {
                self.types
                    .iter()
                    .filter(|t| t.3 == g && t.2 == kind)
                    .map(|t| t.1)
                    .sum::<f32>()
            })
            .collect();
        let dens: Vec<f32> = (0..self.groups.len())
            .map(|g| {
                if kind == LaneKind::Street {
                    let here = lane
                        .and_then(|i| self.net.lanes.get(i))
                        .map(|l| self.lane_group_density(l, g))
                        .unwrap_or(1.0);
                    self.group_density(g) * here
                } else {
                    1.0
                }
            })
            .collect();
        // (and only the vehicles the lane is open to: Grundorf's trucks where it says
        // `trucks`, Omsi.exe 0x71d714)
        let barred: Vec<*const VehicleType> = match lane.filter(|_| kind == LaneKind::Street).and_then(|i| self.net.lanes.get(i)) {
            Some(l) => self.types.iter().filter(|t| !l.allows(t.0.def.ai_veh_type)).map(|t| Arc::as_ptr(&t.0)).collect(),
            None => Vec::new(),
        };
        let weight = |t: &(Arc<VehicleType>, f32, LaneKind, usize)| -> f32 {
            let gw = group_weight.get(t.3).copied().unwrap_or(0.0);
            if gw <= 0.0 || barred.contains(&Arc::as_ptr(&t.0)) {
                0.0
            } else {
                t.1 / gw * dens.get(t.3).copied().unwrap_or(0.0)
            }
        };
        let total: f32 = self.types.iter().filter(|t| t.2 == kind).map(weight).sum();
        if total <= 0.0 {
            return None;
        }
        let mut x = self.rand_f() as f32 * total;
        for t in self.types.iter().filter(|t| t.2 == kind) {
            let w = weight(t);
            if x < w {
                return Some(t.0.clone());
            }
            x -= w;
        }
        self.types
            .iter()
            .filter(|t| t.2 == kind)
            .last()
            .map(|t| t.0.clone())
    }

    /// Put parked cars onto the lanes they stand in or beside: (distance along the lane,
    /// signed lateral offset, + = right). `cars` are the ones the tiles placed since the last
    /// call; the cars no lane was found for before are tried again where the lanes `added`
    /// just came in.
    fn sort_parked(&mut self, cars: Vec<(DVec3, f64)>, added: std::ops::Range<usize>) {
        let mut todo: Vec<DVec3> = Vec::new();
        if !added.is_empty() && !self.parked_waiting.is_empty() {
            let cells: hashbrown::HashSet<(i32, i32)> = added
                .clone()
                .flat_map(|i| Network::lane_cells(&self.net.lanes[i]))
                .collect();
            let near = |p: &DVec3| {
                let (cx, cy) = Network::grid_cell(*p);
                (-1..=1).any(|dx| (-1..=1).any(|dy| cells.contains(&(cx + dx, cy + dy))))
            };
            let (retry, keep): (Vec<DVec3>, Vec<DVec3>) = std::mem::take(&mut self.parked_waiting)
                .into_iter()
                .partition(|p| near(p));
            self.parked_waiting = keep;
            todo = retry;
        }
        let new_cars = cars.len();
        todo.extend(cars.into_iter().map(|(p, _heading)| p));
        if todo.is_empty() {
            return;
        }
        let mut on_lanes = 0usize;
        for p in todo {
            // a car beside a lane is within a few metres of it: the lanes of the cells
            // around it are enough, and a car in a car park finds none
            let beside = self
                .net
                .nearest_lane_near(p, LaneKind::Street)
                .filter(|(l, _, d)| *d <= (self.net.lanes[*l].width * 0.5).max(1.5) as f64 + 1.6);
            let Some((l, s, _)) = beside else {
                self.parked_waiting.push(p);
                continue;
            };
            let (q, h) = self.net.lanes[l].at(s);
            let hr = (h as f64).to_radians();
            let right = DVec3::new(hr.cos(), -hr.sin(), 0.0);
            let lat = (p - q).dot(right) as f32;
            if lat.abs() < 0.9 && omsi_cfg::env::var_os("OMSI_DEBUG_TRAFFIC").is_some() {
                let lane = &self.net.lanes[l];
                log::info!("parked car at ({:.1}, {:.1}) stands in lane {l} ({} {:?}, width {:.1}, heading {:.0} there, s {s:.1} of {:.1}, {lat:+.2} m to the side)", p.x, p.y, lane.name, lane.key, lane.width, h, lane.length());
            }
            self.parked.entry(l).or_default().push((s, lat));
            on_lanes += 1;
        }
        if omsi_cfg::env::var_os("OMSI_DEBUG_TRAFFIC").is_some() {
            log::info!("traffic: {new_cars} parked cars placed, {on_lanes} more stand in or beside a lane ({} in all, {} not beside one)", self.parked.values().map(|v| v.len()).sum::<usize>(), self.parked_waiting.len());
        }
    }

    /// Is a vehicle of radius `r` at `p` hidden from the viewer by buildings (or a hill)?
    /// Every line of sight to it must be: to its middle, to both ends whichever way it
    /// points and over its roof. A single line to the middle let a car come and go half
    /// out from behind a corner, in plain view.
    fn occluded(&self, world: &World, v: &Viewer, p: DVec3, r: f64) -> bool {
        let rel = (p - v.pos).truncate();
        let across = if rel.length() > 1e-3 {
            DVec3::new(-rel.y, rel.x, 0.0).normalize()
        } else {
            DVec3::X
        };
        let along = DVec3::new(rel.x, rel.y, 0.0).normalize_or_zero();
        let reach = (r * 0.8).max(1.5);
        let mut targets = vec![
            p + DVec3::new(0.0, 0.0, 1.2),
            p + across * reach + DVec3::new(0.0, 0.0, 1.0),
            p - across * reach + DVec3::new(0.0, 0.0, 1.0),
            p - along * reach + DVec3::new(0.0, 0.0, 1.0),
        ];
        // the roof of a bus or a lorry shows over a wall a car hides behind
        if r > 4.0 {
            targets.push(p + DVec3::new(0.0, 0.0, 3.2));
        }
        targets.into_iter().all(|t| self.sight_blocked(world, v, t))
    }

    /// Is the line of sight from the viewer to the point `target` blocked by a building
    /// (or a hill)?
    fn sight_blocked(&self, world: &World, v: &Viewer, target: DVec3) -> bool {
        let p = target;
        let blocker = match &self.occluders {
            Some(c) => c.ray_blocker(v.pos, target, 2.5, 3.0),
            None => world.collision.lock().ray_blocker(v.pos, target, 2.5, 3.0),
        };
        if let Some(b) = blocker {
            if self.debug_population {
                log::info!("  line of sight to ({:.0}, {:.0}) blocked by a box at ({:.1}, {:.1}) {:.1} x {:.1} m, z {:.1}..{:.1}", p.x, p.y, b.center.x, b.center.y, b.half.x * 2.0, b.half.y * 2.0, b.z0, b.z1);
            }
            return true;
        }
        // the ground between: a crest or an embankment
        for k in 1..8 {
            let t = k as f64 / 8.0;
            let q = v.pos.lerp(target, t);
            if let Some(g) = terrain_height(world, q.x, q.y) {
                if g > q.z + 0.5 {
                    if self.debug_population {
                        log::info!("  line of sight to ({:.0}, {:.0}) blocked by the ground at ({:.0}, {:.0}): {:.1} over {:.1}", p.x, p.y, q.x, q.y, g, q.z);
                    }
                    return true;
                }
            }
        }
        false
    }

    /// May a vehicle be put on the road at `p` without the player seeing it appear? A
    /// timetable bus asks this before it spawns mid-route.
    pub fn may_appear(&self, world: &World, p: DVec3) -> bool {
        self.initial || self.hidden(world, p, 8.0)
    }

    /// The world is still being built (the first populate, the first seconds): vehicles
    /// may be put anywhere.
    pub fn loading_phase(&self) -> bool {
        self.initial || self.time < 2.0
    }

    /// Could the player not see a vehicle (radius `r`) at `p` appear or vanish? Never close
    /// by: mirrors, a turn of the head and the gaps between houses see what is near,
    /// whatever the collision boxes say (a bus let appear 40 m away behind a box that stood
    /// for a building with a gateway in it was seen popping up in the middle of the street).
    /// Within `NEAR_HIDE` only behind a building or the ground, wherever the camera looks -
    /// the mirrors look back, and the head turns: buses appeared 160 m behind the player
    /// in plain sight of the mirrors, and a bus waiting at the edge of the loaded route
    /// vanished beside the player's bus because the camera was looking ahead. Further off:
    /// beyond what is drawn, out of the picture, or behind something.
    pub fn hidden(&self, world: &World, p: DVec3, r: f64) -> bool {
        let Some(v) = self.viewer else { return true };
        let d = (p - v.pos).length();
        if d < NEVER_VANISH_WITHIN {
            return false;
        }
        if !v.draws(d, r) {
            return true;
        }
        if d < NEAR_HIDE {
            return self.occluded(world, &v, p, r);
        }
        !v.frames(p, r) || self.occluded(world, &v, p, r)
    }

    /// Spawn cars until `target` are within `spawn_radius` of `center`; despawn far ones.
    pub fn populate(
        &mut self,
        world: &World,
        renderer: &Renderer,
        scene: &mut Scene,
        center: DVec3,
    ) {
        self.populate_seen(world, renderer, scene, center, None);
    }

    /// Keep the population around the player: cars are taken off only where nobody sees
    /// it (far away and out of view, or behind a building), and new ones appear only there.
    /// `view` (a unit vector) stands in for the viewer when none has been set.
    pub fn populate_seen(
        &mut self,
        world: &World,
        renderer: &Renderer,
        scene: &mut Scene,
        center: DVec3,
        view: Option<DVec3>,
    ) {
        if self.mirror {
            return;
        }
        if self.viewer.is_none() {
            if let Some(f) = view {
                self.viewer = Some(Viewer {
                    pos: center,
                    forward: f,
                    tan_x: 1.2,
                    tan_y: 0.6,
                    range: VISIBLE_RANGE,
                    min_size: 0.0,
                    max_dist: 0.0,
                    fov: 1.0,
                });
            }
        }
        let far = self.spawn_radius * DESPAWN_FACTOR;
        self.advance_dormant();
        // the cars somebody stands behind
        let queued: std::collections::HashSet<u64> = self.cars.iter().filter(|c| c.stopped > 5.0).filter_map(|c| c.lead_car).collect();
        let mut i = 0;
        let mut off_ground = 0usize;
        let mut asleep = 0usize;
        while i < self.cars.len() {
            let c = &self.cars[i];
            let p = c.vehicle.position;
            // (the nearest player: a LAN host keeps the traffic around the others too)
            let dist = self
                .lan_centers
                .iter()
                .fold((p - center).length(), |d, o| d.min((p - *o).length()));
            // every car on the road or the rails whose ground has been unloaded goes: the
            // lanes stay in the network when their tile goes, but nothing may drive over
            // ground that is not there (tiles only go well beyond the view, timetable buses
            // included; their trips come back with the tiles)
            let flying = self
                .net
                .lanes
                .get(c.state.lane)
                .map(|l| l.kind == LaneKind::Air)
                .unwrap_or(false);
            let unloaded = !flying && !world.has_ground(p.x, p.y);
            off_ground += unloaded as usize;
            let random = !c.is_bus() || c.gone;
            let r = (c.state.length as f64 * 0.5).max(2.0);
            // standing at the end of the network (the map's edge): Omsi.exe never lets a
            // random car stand there - once 0x71dc9c finds no next segment (0x612e10) its
            // segment stays -1 and 0x6fe3fc deletes it the same frame (0x703bb0); a bus
            // that gave up goes once out of sight, or too far off for the renderer to draw it
            let at_end = c.gone
                && c.stopped > if c.is_bus() { 20.0 } else { 0.5 }
                && c.state.route.is_empty()
                && self.net.lanes[c.state.lane].next.is_empty();
            let from_eye = self.viewer.map(|v| (p - v.pos).length()).unwrap_or(dist);
            // a timetable bus waiting where the loaded part of its route ends
            let at_edge = c.route_open()
                && c.state.speed < 0.1
                && c.state.route.last() == Some(&c.state.lane)
                && c.state.s > self.net.lanes[c.state.lane].length() - 25.0;
            // a random car still on its way that goes out of range sleeps instead (see
            // `DormantCar`); only one whose trip is over leaves the map
            let sleeps_instead = random && !c.gone && !c.is_bus() && self.net.lanes.get(c.state.lane).map(|l| l.kind == LaneKind::Street).unwrap_or(false);
            let remove = if unloaded {
                true
            } else if at_edge {
                // (its trip comes back with the tiles; kept until nobody saw it, a bus stood
                // with its passengers at the far end of a straight road for good)
                self.hidden(world, p, r) || (c.stopped > 8.0 && from_eye > 180.0) || c.stopped > 150.0
            } else if !random {
                false
            } else if at_end && (!c.is_bus() || self.hidden(world, p, r) || (c.stopped > 8.0 && from_eye > 180.0) || c.stopped > 150.0 || (c.stopped > 25.0 && queued.contains(&c.id) && from_eye > 25.0)) {
                // (and in view too once others wait behind it: a fire engine at the end of a
                // dead-end street held a queue of fourteen cars for two and a half minutes)
                // (taken at once it vanished in plain view 300 m ahead; but a car kept
                // until nobody could see it stood for good at the end of a long straight
                // road in view, and the queue behind it - timetable buses with their
                // passengers among them - never moved again)
                true
            } else if c.gone || dist > far {
                // in plain view a car stays until the renderer leaves it out anyway, and
                // close by (the mirrors, a turn of the head) it stays in any case
                // (one that gave up in a gridlock goes after four minutes even in view,
                // unless right beside the viewer: kept until nobody saw it, a jam at a
                // junction the player watched never cleared; and one held for a minute and
                // a half by nothing anybody can see - no car, bus or light in front of it,
                // nobody it gives way to - goes then: whatever held it, it stood against
                // an invisible wall with the traffic queued up behind it for as long as the
                // player looked)
                (dist > VISIBLE_RANGE * 1.3 && from_eye > NEAR_HIDE)
                    || self.hidden(world, p, r)
                    || (c.gone && c.stopped > 240.0 && from_eye > 40.0)
                    || (c.gone && c.progress.1 > PHANTOM_WAIT && matches!(c.why.0, "" | "parked" | "people") && from_eye > 25.0)
            } else {
                false
            };
            if remove && sleeps_instead && !at_end {
                let c = self.cars.swap_remove(i);
                asleep += 1;
                self.dormant.push(DormantCar {
                    id: c.id,
                    ty: c.vehicle.ty.clone(),
                    kind: LaneKind::Street,
                    lane: c.state.lane,
                    s: c.state.s,
                    speed: c.state.speed.max(2.0),
                    seed: c.seed,
                    scheme: c.scheme,
                    walk: c.seed ^ c.id.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
                });
                self.orphan_sounds.extend(c.sounds);
                for r in std::iter::once(c.render).chain(c.trailer_renders) {
                    world.release_vehicle(renderer, scene, r);
                }
            } else if remove {
                let c = self.cars.swap_remove(i);
                if c.is_bus() && (unloaded || at_edge) {
                    self.removed_scheduled.push(c.id);
                }
                if self.debug_population {
                    let v = self.viewer;
                    log::info!("population t={:.1}: car {} removed at ({:.0}, {:.0}), {:.0} m from the player, in frame {}, behind a building {}, {}", self.time, c.id, p.x, p.y, dist, v.map(|v| v.frames(p, r)).unwrap_or(false), v.map(|v| self.occluded(world, &v, p, r)).unwrap_or(false), if c.gone { "finished" } else { "far away" });
                }
                self.orphan_sounds.extend(c.sounds);
                for r in std::iter::once(c.render).chain(c.trailer_renders) {
                    world.release_vehicle(renderer, scene, r);
                }
            } else {
                i += 1;
            }
        }
        if off_ground > 0 && omsi_cfg::env::var_os("OMSI_DEBUG_TRAFFIC").is_some() {
            log::info!("traffic: {off_ground} vehicles taken away with the tiles under them");
        }
        if (asleep > 0 || !self.dormant.is_empty()) && self.debug_population {
            log::info!("population t={:.1}: {asleep} cars went out of range and drive on unseen; {} on the map out of range, {} in range", self.time, self.dormant.len(), self.cars.len());
        }
        if self.types.is_empty() || self.net.lanes.is_empty() {
            self.initial = false;
            return;
        }
        // made only for the lights: nothing new while the target is 0, but the cars of a
        // target raised and lowered again go as they do anywhere (returning before the loop
        // above, they stood at the map's edge and drove over unloaded tiles for good)
        if self.lights_only && self.target == 0 {
            return;
        }
        // aircraft: a few on the flight paths, independent of the street target
        let has_air = self.types.iter().any(|t| t.2 == LaneKind::Air);
        // the map's traffic density by hour (and group) scales the street traffic ...
        let density = self.street_density();
        // ... and so does how much road there is around: the same number of cars looks
        // empty on a six-lane Berlin junction and crowded on a village lane, so the count
        // asked for is per a neighbourhood of about 250 lanes; and as Omsi spawns on each
        // path at a rate of its [rule] trafficdensity, paths of low density bring fewer cars
        // and those of density 0 (or kept clear of cars) none
        let near_density: Vec<f32> = self.net.lanes_starting_near(center, self.spawn_radius)
            .into_iter()
            .map(|i| &self.net.lanes[i])
            .filter(|l| {
                l.kind == LaneKind::Street
                    && l.points
                        .first()
                        .map(|p| (*p - center).length() < self.spawn_radius)
                        .unwrap_or(false)
            })
            .map(|l| if l.no_cars { 0.0 } else { l.density.clamp(0.0, 4.0) })
            .collect();
        let street_target = (self.target as f32 * density * road_scale(&near_density)).round() as usize;
        // the cars that come into range again, where they have got to
        self.wake_dormant(world, renderer, scene, center, street_target);
        // the whole map's population: as dense as around the player, on every street the
        // map has shown so far (sleeping where the player is not)
        self.fill_map(center, street_target);
        for (kind, target) in [
            (LaneKind::Street, street_target),
            (LaneKind::Air, if has_air { 3 } else { 0 }),
        ] {
            // (a LAN host counts the cars round itself only: counted over the whole map,
            // the traffic it keeps round the other players met its own target and the
            // host drove through empty streets, #342)
            if kind == LaneKind::Street && !self.lan_centers.is_empty() {
                self.count_near = Some((center, self.spawn_radius));
            }
            self.populate_kind(world, renderer, scene, center, kind, target);
        }
        self.populate_lan_centers(world, renderer, scene, center, street_target);
        if !self.initial {
            self.pull_out_parked(world, renderer, scene, center);
            self.park_in(world, center);
        }
        self.initial = false;
    }

    /// Now and then a car near the player parks: a space at the kerb that a parked car has
    /// left is taken by a car of the same kind driving up the lane beside it - it indicates,
    /// slows down, stops beside the space, moves over into it and stands there as the
    /// parked car it was. Called with every population pass (about every two seconds).
    pub fn park_in(&mut self, world: &World, center: DVec3) {
        let forced = omsi_cfg::env::var("OMSI_PARK_IN").ok().and_then(|v| v.parse::<f64>().ok());
        if self.rand_f() >= forced.unwrap_or(0.04) {
            return;
        }
        let debug = omsi_cfg::env::var_os("OMSI_DEBUG_TRAFFIC").is_some() || forced.is_some();
        let taken: hashbrown::HashSet<i64> = self.cars.iter().filter_map(|c| c.park.map(|p| p.key)).collect();
        let mut spots = world.free_parking();
        spots.retain(|(k, p)| !taken.contains(k) && (30.0..260.0).contains(&(p.pos - center).truncate().length()));
        spots.sort_by_key(|s| s.0);
        let mut why: Vec<String> = Vec::new();
        while !spots.is_empty() {
            let (key, p) = spots.swap_remove(self.rand() as usize % spots.len());
            let Some((l, s, _)) = self.net.nearest_lane_near(p.pos, LaneKind::Street) else { continue };
            let lane = &self.net.lanes[l];
            if lane.no_cars || s < 8.0 || s > lane.length() - 4.0 {
                continue;
            }
            let (q, h) = lane.at(s);
            let hr = (h as f64).to_radians();
            let lat = (p.pos - q).dot(DVec3::new(hr.cos(), -hr.sin(), 0.0)) as f32;
            let mut dh = (p.heading - h as f64).rem_euclid(360.0);
            if dh > 180.0 {
                dh -= 360.0;
            }
            // beside the lane on the right and in line with it (a space across the kerb or in
            // a row is not driven into)
            if !(1.2..4.2).contains(&lat) || dh.abs() > 12.0 {
                why.push(format!("space {key}: {lat:+.1} m beside lane {l}, {dh:+.0} deg"));
                continue;
            }
            let folder = p.sco.parent().map(|d| d.to_string_lossy().to_lowercase());
            // a car of that kind coming up the lane, far enough off to slow down gently
            let mut best: Option<(usize, f32)> = None;
            for (i, c) in self.cars.iter().enumerate() {
                if c.is_bus() || c.gone || c.park.is_some() || c.passing.is_some() || c.state.change.is_some() || c.pull_out > 0.0 {
                    continue;
                }
                if c.vehicle.ty.def.path.parent().map(|d| d.to_string_lossy().to_lowercase()) != folder {
                    continue;
                }
                if c.state.speed > 15.0 || c.state.lateral.abs() > 0.2 {
                    continue;
                }
                let Some(&(_, dl)) = self.way_lanes(&c.state, 160.0).iter().find(|w| w.0 == l) else { continue };
                let d = dl + s;
                let need = c.state.speed * c.state.speed / 2.0 + 20.0;
                if d > need && d < 150.0 && best.map(|b| d < b.1).unwrap_or(true) {
                    best = Some((i, d));
                }
            }
            let Some((i, d)) = best else {
                why.push(format!("space {key}: no car of its kind coming up lane {l}"));
                continue;
            };
            let car = &mut self.cars[i];
            car.park = Some(ParkPlan { key, lane: l, s, lat, ramped: false, done: false });
            // (it parks: no more route to plan than to here)
            car.gone = false;
            if debug {
                log::info!("car {} parks in the space of parked car {key} ({:.0} m ahead, {lat:.1} m right of lane {l})", car.id, d);
            }
            return;
        }
        if debug && !why.is_empty() {
            log::info!("park-in: none this time ({})", why.join("; "));
        }
    }

    /// Now and then a car parked at the kerb near the player drives off: the parked object
    /// goes (its space stays empty) and the AI car of the same folder takes its place, in
    /// the parking position, indicating, and pulls out into its lane once the road behind
    /// it is clear. Called with every population pass (about every two seconds).
    pub fn pull_out_parked(&mut self, world: &World, renderer: &Renderer, scene: &mut Scene, center: DVec3) {
        let forced = omsi_cfg::env::var("OMSI_PARKED_PULL_OUT").ok().and_then(|v| v.parse::<f64>().ok());
        // about one car a minute
        if self.rand_f() >= forced.unwrap_or(0.035) {
            return;
        }
        let debug = omsi_cfg::env::var_os("OMSI_DEBUG_TRAFFIC").is_some() || forced.is_some();
        let mut candidates: Vec<(i64, crate::scene::ParkedObject)> = world
            .parked_objects
            .lock()
            .iter()
            .filter(|(_, p)| {
                let d = (p.pos - center).truncate().length();
                (25.0..180.0).contains(&d)
            })
            .map(|(k, p)| (*k, p.clone()))
            .collect();
        candidates.sort_by_key(|c| c.0);
        if debug {
            log::info!("parked pull-out: {} parked cars in range ({} loaded)", candidates.len(), world.parked_objects.lock().len());
        }
        let mut why: Vec<String> = Vec::new();
        let mut tried = 0;
        while !candidates.is_empty() && tried < 8 {
            tried += 1;
            let (key, p) = candidates.swap_remove(self.rand() as usize % candidates.len());
            // the AI car of the same folder (a parked Golf is `parked_vw_golf_2.sco` next to
            // `ai_vw_golf_2.bus`)
            let folder = p.sco.parent().map(|d| d.to_string_lossy().to_lowercase());
            let Some(ty) = self
                .types
                .iter()
                .filter(|t| t.2 == LaneKind::Street)
                .find(|t| t.0.def.path.parent().map(|d| d.to_string_lossy().to_lowercase()) == folder)
                .map(|t| t.0.clone())
            else {
                why.push(format!("no AI car in {folder:?}"));
                continue;
            };
            let Some((l, s, _)) = self.net.nearest_lane_near(p.pos, LaneKind::Street) else {
                why.push("no lane".into());
                continue;
            };
            let lane = &self.net.lanes[l];
            if lane.no_cars || s < 4.0 || s > lane.length() - 4.0 {
                continue;
            }
            let (q, h) = lane.at(s);
            let hr = (h as f64).to_radians();
            let right = DVec3::new(hr.cos(), -hr.sin(), 0.0);
            let lat = (p.pos - q).dot(right) as f32;
            // beside the lane on the right, facing its way (one parked in the lane itself stands
            // bumper to bumper in a row it cannot steer out of)
            let mut dh = (p.heading - h as f64).rem_euclid(360.0);
            if dh > 180.0 {
                dh -= 360.0;
            }
            if !(0.8..4.5).contains(&lat) || dh.abs() > 30.0 {
                why.push(format!("{lat:+.1} m beside lane {l}, {dh:+.0} deg to it"));
                continue;
            }
            // nobody close by on the road
            if self.cars.iter().any(|c| (c.vehicle.position - p.pos).length() < 30.0) || !self.spawn_clear(&ty, q, h as f64) {
                why.push("road not clear".into());
                continue;
            }
            if world.depart_parked(renderer, scene, key).is_none() {
                continue;
            }
            if let Some(list) = self.parked.get_mut(&l) {
                if let Some(j) = (0..list.len()).min_by(|&a, &b| (list[a].0 - s).abs().total_cmp(&(list[b].0 - s).abs())) {
                    if (list[j].0 - s).abs() < 3.0 {
                        list.swap_remove(j);
                    }
                }
            }
            let seed = self.rand();
            let id = self.create_car(world, renderer, scene, center, LaneKind::Street, l, s, ty.clone(), seed, None, None, Some(0.0), None);
            let net = &self.net;
            if let Some(car) = self.cars.iter_mut().find(|c| c.id == id) {
                car.state.lateral = lat;
                car.state.lateral_target = 0.0;
                car.state.lateral_ramp = (lat, 0.0, car.state.odometer, (lat * 6.0).clamp(8.0, 16.0));
                car.body = place_body(net, &car.state, &mut car.vehicle, MotionKind::Road);
                car.pull_out = 2.0 + (seed % 1000) as f32 / 400.0;
                car.state.blinker = 1;
            }
            if debug {
                log::info!("parked car {key} ({}) at ({:.1}, {:.1}) pulls out as car {id}, {lat:.1} m right of lane {l}", ty.def.path.display(), p.pos.x, p.pos.y);
            }
            return;
        }
        if debug && !why.is_empty() {
            log::info!("parked pull-out: none this time ({})", why.join("; "));
        }
    }

    fn populate_kind(
        &mut self,
        world: &World,
        renderer: &Renderer,
        scene: &mut Scene,
        center: DVec3,
        kind: LaneKind,
        target: usize,
    ) {
        let radius = if kind == LaneKind::Air {
            self.spawn_radius * 6.0
        } else {
            self.spawn_radius
        };
        let nearby = self.net.lanes_starting_near(center, radius);
        // candidate lanes of this kind near the centre
        let pick = |through: bool| -> Vec<(usize, f32)> {
            nearby.iter().copied()
                .map(|i| (i, &self.net.lanes[i]))
                .filter(|(_, l)| {
                    l.kind == kind
                        && l.length() > 8.0
                        && (l.start() - center).truncate().length() < radius
                })
                // lanes the map keeps clear of cars, and those whose [rule] trafficdensity is
                // zero, are not spawned on at all; a lower density makes a lane that much less
                // likely to be picked
                .filter(|(_, l)| !l.no_cars && l.density > 0.0)
                // nor, where there are others, lanes that end the network just ahead (the car
                // would only drive into the end and wait there to be taken away)
                .filter(|(i, _)| {
                    !through
                        || kind != LaneKind::Street
                        || self
                            .net
                            .reach
                            .get(*i)
                            .map(|r| *r >= omsi_sim::traffic::DEAD_END)
                            .unwrap_or(true)
                })
                // as many cars on a lane as metres of it (times its density): counted per
                // lane, the many short lanes of a junction drew the cars into the town's
                // tangles and left the long roads between them empty
                .map(|(i, l)| (i, l.length() * l.density.clamp(0.05, 4.0)))
                .collect::<Vec<(usize, f32)>>()
        };
        let mut candidates = pick(true);
        if candidates.is_empty() {
            candidates = pick(false);
        }
        if candidates.is_empty() {
            return;
        }
        let mut acc = 0.0f32;
        let cumulative: Vec<f32> = candidates.iter().map(|c| { acc += c.1; acc }).collect();
        let total_w = acc.max(1e-3);
        let mut attempts = 0;
        let counted_near = self.count_near.take();
        let unscheduled = self
            .cars
            .iter()
            .filter(|c| {
                !c.is_bus()
                    && !c.gone
                    && counted_near
                        .map(|(p, r)| (c.vehicle.position - p).length() < r)
                        .unwrap_or(true)
                    && self
                        .net
                        .lanes
                        .get(c.state.lane)
                        .map(|l| l.kind == kind)
                        .unwrap_or(false)
            })
            .count();
        let mut count = unscheduled;
        while count < target && attempts < target * 12 {
            attempts += 1;
            let x = self.rand_f() as f32 * total_w;
            let lane = candidates[cumulative.partition_point(|&c| c < x).min(candidates.len() - 1)].0;
            let s = (self.rand_f() * (self.net.lanes[lane].length() as f64 - 6.0)) as f32 + 3.0;
            let (p, _) = self.net.lanes[lane].at(s);
            let rel = p - center;
            if rel.length() < 40.0 {
                continue; // not right next to the player
            }
            // nobody may see it appear (the first population is the world as it loads)
            if kind == LaneKind::Street && !self.initial && !self.may_appear(world, p) {
                continue;
            }
            if self
                .cars
                .iter()
                .any(|c| (c.vehicle.position - p).length() < 14.0)
            {
                continue;
            }
            // only on loaded ground (a lane's tile may have gone again)
            if kind != LaneKind::Air && !world.has_ground(p.x, p.y) {
                continue;
            }
            let heading = self.net.lanes[lane].at(s).1 as f64;
            // nor just in front of one driving up to that place (it would have to stop hard)
            let in_front_of_someone = self.cars.iter().any(|c| {
                let rel = p - c.vehicle.position;
                let h = c.vehicle.heading.to_radians();
                let (along, across) = (
                    rel.x * h.sin() + rel.y * h.cos(),
                    (rel.x * h.cos() - rel.y * h.sin()).abs(),
                );
                along > 0.0
                    && along
                        < 20.0
                            + (c.state.speed * c.state.speed / (2.0 * c.state.decel.max(1.0)))
                                as f64
                                * 1.5
                    && across < 3.0
            });
            if in_front_of_someone {
                continue;
            }
            if self
                .parked
                .get(&lane)
                .map(|l| {
                    l.iter()
                        .any(|&(ps, lat)| (ps - s).abs() < 8.0 && lat.abs() < 1.5)
                })
                .unwrap_or(false)
            {
                continue; // not into a car parked in the lane
            }
            // (a lane may carry none of the groups that drive now: try another)
            let Some(ty) = self.pick_type(kind, Some(lane)) else {
                continue;
            };
            // (nor onto the rear section of an articulated bus, nor the player's bus)
            if kind != LaneKind::Air && !self.spawn_clear(&ty, p, heading) {
                continue;
            }
            let seed = self.rand();
            self.create_car(world, renderer, scene, center, kind, lane, s, ty, seed, None, None, None, None);
            count += 1;
        }
    }

    /// The cars out of range drive on: along their lanes at about the lanes' speed, taking
    /// a random way at every fork; one that reaches the end of the network has left the map.
    fn advance_dormant(&mut self) {
        let dt = (self.time - self.dormant_time).clamp(0.0, 10.0);
        self.dormant_time = self.time;
        if dt <= 0.0 || self.dormant.is_empty() {
            return;
        }
        let mut i = 0;
        while i < self.dormant.len() {
            let mut gone = false;
            {
                let d = &mut self.dormant[i];
                let lane = &self.net.lanes[d.lane];
                // (waits at lights and junctions taken as a quarter off the speed limit)
                d.speed = (lane.speed_limit_kmh.min(60.0) / 3.6 * 0.75).max(2.0);
                d.s += d.speed * dt;
                let mut guard = 0;
                while d.s > self.net.lanes[d.lane].length() && guard < 32 {
                    guard += 1;
                    let l = &self.net.lanes[d.lane];
                    // the ways a car on the road would take (`AiState::choose_after`): those
                    // of its group, else those open to cars, else any - only where the
                    // network ends does it leave the map (filtering by its group alone, the
                    // trucks of Spandau were gone at the first junction whose turn has no
                    // `trucks` rule, and hardly one of them ever came into range)
                    let pool = self.types.iter().find(|t| Arc::ptr_eq(&t.0, &d.ty)).and_then(|t| self.group_uvg[t.3]);
                    let same_kind = |n: &usize| self.net.lanes[*n].kind == d.kind;
                    let open = |pooled: bool| -> Vec<usize> {
                        l.next
                            .iter()
                            .copied()
                            .filter(same_kind)
                            .filter(|&n| {
                                let nl = &self.net.lanes[n];
                                let density = match pool.filter(|_| pooled) {
                                    Some(p) => nl.pool_density(&self.uvg_defaults, p),
                                    None => nl.density,
                                };
                                nl.allows(d.ty.def.ai_veh_type) && density > 0.0
                            })
                            .collect()
                    };
                    let mut options = if pool.is_some() { open(true) } else { Vec::new() };
                    if options.is_empty() {
                        options = open(false);
                    }
                    if options.is_empty() {
                        options = l.next.iter().copied().filter(same_kind).collect();
                    }
                    if options.is_empty() {
                        gone = true;
                        break;
                    }
                    d.s -= l.length();
                    d.walk = d.walk.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    d.lane = options[(d.walk >> 33) as usize % options.len()];
                }
            }
            if gone {
                self.dormant.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }

    /// The cars out of range that have come near again take their bodies back - where
    /// nobody sees it happen, up to a little over the number asked for around the player.
    fn wake_dormant(&mut self, world: &World, renderer: &Renderer, scene: &mut Scene, center: DVec3, target: usize) {
        if self.dormant.is_empty() {
            return;
        }
        let active = self.cars.iter().filter(|c| !c.is_bus() && !c.gone).count();
        let mut budget = (target as f32 * 1.25).ceil() as usize;
        budget = budget.saturating_sub(active);
        let centers: Vec<DVec3> = std::iter::once(center).chain(self.lan_centers.iter().copied()).collect();
        let mut i = 0;
        while i < self.dormant.len() && budget > 0 {
            let (p, h, ty) = {
                let d = &self.dormant[i];
                let l = &self.net.lanes[d.lane];
                let (p, h) = l.at(d.s.clamp(0.0, (l.length() - 0.1).max(0.0)));
                (p, h as f64, d.ty.clone())
            };
            let near = centers.iter().any(|c| (p - *c).truncate().length() < self.spawn_radius);
            // (as a new car: never close by, where the mirrors and a turn of the head see
            // it - woken out of the picture 30-60 m from the bus, a car came into being in
            // the mirror or just round the corner)
            let ok = near
                && world.has_ground(p.x, p.y)
                && self.may_appear(world, p)
                && !self.cars.iter().any(|c| (c.vehicle.position - p).length() < 14.0)
                && self.spawn_clear(&ty, p, h);
            if ok {
                let d = self.dormant.swap_remove(i);
                self.create_car(world, renderer, scene, center, d.kind, d.lane, d.s, d.ty, d.seed, Some(d.scheme), Some(d.id), Some(d.speed), None);
                budget -= 1;
            } else {
                i += 1;
            }
        }
    }

    /// Fill the map: the streets the map has shown so far carry as many cars per metre as
    /// the ones around the player, the ones out of range as dormant cars (see `DormantCar`).
    fn fill_map(&mut self, center: DVec3, street_target: usize) {
        if street_target == 0 {
            return;
        }
        let far = self.spawn_radius * DESPAWN_FACTOR;
        let centers: Vec<DVec3> = std::iter::once(center).chain(self.lan_centers.iter().copied()).collect();
        let mut nearby: Vec<usize> = centers.iter()
            .flat_map(|&c| self.net.lanes_starting_near(c, self.spawn_radius))
            .collect();
        nearby.sort_unstable();
        nearby.dedup();
        let mut near = 0f64;
        for i in nearby {
            let l = &self.net.lanes[i];
            let Some(w) = street_lane_weight(l) else { continue };
            let d = centers.iter().map(|c| (l.start() - *c).truncate().length()).fold(f64::MAX, f64::min);
            if d < self.spawn_radius {
                near += w;
            }
        }
        if near < 50.0 {
            return;
        }
        let map_target = ((street_target as f64 * self.street_weight / near).min(street_target as f64 * MAP_POPULATION_FACTOR as f64)) as usize;
        let present = self.cars.iter().filter(|c| !c.is_bus() && !c.gone).count() + self.dormant.len();
        if present >= map_target {
            return;
        }
        // The full outside list is only needed while replenishing the map population.
        let outside: Vec<(usize, f32)> = self.net.lanes.iter().enumerate()
            .filter_map(|(i, l)| {
                let w = street_lane_weight(l)?;
                let d = centers.iter().map(|c| (l.start() - *c).truncate().length()).fold(f64::MAX, f64::min);
                (d > far).then_some((i, w as f32))
            })
            .collect();
        if outside.is_empty() {
            return;
        }
        let mut acc = 0.0f32;
        let cumulative: Vec<f32> = outside.iter().map(|c| { acc += c.1; acc }).collect();
        for _ in 0..(map_target - present).min(64) {
            let x = self.rand_f() as f32 * acc;
            let lane = outside[cumulative.partition_point(|&c| c < x).min(outside.len() - 1)].0;
            let s = (self.rand_f() * (self.net.lanes[lane].length() as f64 - 4.0)) as f32 + 2.0;
            let Some(ty) = self.pick_type(LaneKind::Street, Some(lane)) else {
                continue;
            };
            let seed = self.rand();
            let scheme = if ty.paint_schemes.is_empty() { None } else { Some((seed >> 8) as usize % ty.paint_schemes.len().min(AI_SCHEMES)) };
            let id = self.next_id;
            self.next_id += 1;
            self.dormant.push(DormantCar { id, ty, kind: LaneKind::Street, lane, s, speed: 8.0, seed, scheme, walk: seed | 1 });
        }
    }

    /// Put a random car of type `ty` on `lane` at `s` metres into it and return its id:
    /// `scheme` Some = that paint scheme (a car coming back from out of range keeps its
    /// looks), `id` Some = that id, `speed` Some = at about that speed.
    #[allow(clippy::too_many_arguments)]
    fn create_car(
        &mut self,
        world: &World,
        renderer: &Renderer,
        scene: &mut Scene,
        center: DVec3,
        kind: LaneKind,
        lane: usize,
        s: f32,
        ty: Arc<VehicleType>,
        seed: u64,
        scheme: Option<Option<usize>>,
        id: Option<u64>,
        speed: Option<f32>,
        bus: Option<BusSetup>,
    ) -> u64 {
        let mut host = omsi_sim::VehicleHost::new(omsi_sim::SimClock::default());
        host.font_lib = Some(world.fonts.clone());
        if let Some(b) = &bus {
            host.hof = b.hof.clone();
        }
        // random paint scheme / advert (its variables there for the scripts' {init})
        let scheme = match scheme {
            Some(s) => s,
            None if ty.paint_schemes.is_empty() => None,
            None => Some((seed >> 8) as usize % ty.paint_schemes.len().min(AI_SCHEMES)),
        };
        host.paint_scheme = Some(scheme);
        let mut vehicle = VehicleInstance::new(ty.clone(), host);
        if let Some((num, reg)) = bus.as_ref().and_then(|b| b.number.clone()) {
            if let Some(i) = ty.program.str_var("number") {
                vehicle.state.str_vars[i as usize] = num;
            }
            if let Some(i) = ty.program.str_var("ident") {
                vehicle.state.str_vars[i as usize] = reg;
            }
        } else {
            // A vehicle of the random traffic with a `[number]` list takes a number of it at
            // random and the plate beside it, else the plate its mode makes of the number
            // (TRoadVehicleInst.virtual_11 at 0x7e7b51); a free plate is one of the map's
            // registrations.txt.
            let numbers = ty.def.numbers_with_plates();
            if !numbers.is_empty() {
                let (n, plate) = &numbers[(seed.rotate_left(29) % numbers.len() as u64) as usize];
                if let Some(i) = ty.program.str_var("number") {
                    vehicle.state.str_vars[i as usize] = n.clone();
                }
                if ty.def.registration_mode != 1 {
                    if let Some(i) = ty.program.str_var("ident") {
                        vehicle.state.str_vars[i as usize] = if plate.is_empty() { ty.def.plate_of_number(n) } else { plate.clone() };
                    }
                }
            }
            if ty.def.registration_mode == 1 {
                if let (Some(i), Some(reg)) = (ty.program.str_var("ident"), world.free_registration(seed.rotate_left(17))) {
                    vehicle.state.str_vars[i as usize] = reg;
                }
            }
        }
        // aircraft keep the height of their flight path: a ground sampler would pull
        // them down onto the streets
        vehicle.ground = if kind == LaneKind::Air {
            None
        } else {
            Some(ai_ground(world))
        };
        // and what its wheels stand on, asked as the player's are (see `AiBody::settle`); a
        // coupled part (an articulated bus's rear, a lorry's trailer) asks it too, with the
        // height it is at - the plain sampler gave it the deck of a bridge over its road
        // (`OMSI_AI_WAY_ONLY=1`: on the way and the plain sampler, as before - A/B runs)
        vehicle.contact = (kind == LaneKind::Street && omsi_cfg::env::var_os("OMSI_AI_WAY_ONLY").is_none()).then(|| {
            std::sync::Arc::new(crate::scene::DriveGround {
                terrains: world.terrains.clone(),
                surfaces: world.surfaces.clone(),
            }) as std::sync::Arc<dyn omsi_sim::rigid::Ground>
        });
        vehicle.apply_paint_vars(scheme);
        let render = world.add_vehicle_shared(renderer, scene, &ty, scheme, None);
        let trailer_renders =
            self.attach_trailers(world, renderer, scene, &mut vehicle, scheme, &render);
        if !ty.model.text_textures.is_empty() || bus.is_some() {
            vehicle.init_text_textures(&mut world.fonts.lock(), &|p| {
                omsi_texture::decode_file(p)
                    .ok()
                    .map(|i| (i.width, i.height, i.rgba))
            });
        }
        // a rear section's plates and numbers are `[texttexture]`s of its own reading the
        // leading vehicle's strings (`TrailerPart::update_text_textures`), so they need the
        // same fonts the front's do
        for t in vehicle.trailers.iter_mut() {
            t.init_text_textures(&mut world.fonts.lock(), &|p| {
                omsi_texture::decode_file(p)
                    .ok()
                    .map(|i| (i.width, i.height, i.rgba))
            });
        }
        let mut state = AiState::new(lane, s, seed);
        state.veh_type = if bus.is_some() { -1 } else { ty.def.ai_veh_type };
        if bus.is_none() {
            state.traffic_pool = self.types.iter().find(|t| Arc::ptr_eq(&t.0, &ty))
                .and_then(|t| self.group_uvg[t.3])
                .map(|pool| (pool, self.uvg_defaults.clone()));
        }
        state.plan_next(&self.net);
        // heavy vehicles (trucks, vans) cruise slower, which is what gets them overtaken
        let heavy = ty.def.mass > 6.0 || bus.is_some();
        personality(&mut state, seed, heavy);
        state.max_speed_kmh = if kind == LaneKind::Air {
            AIRCRAFT_KMH
        } else if bus.is_some() {
            // a bus driver keeps to the limit (the town's 50) like the cars round him,
            // with a little more on the arterial roads
            56.0 + (seed % 7) as f32
        } else if heavy {
            // (a truck keeps to the limit like the cars, up to a truck's own 80-90 km/h: it
            // took 38-47 on every road, crawling along 80 km/h roads, #327)
            80.0 + (seed % 10) as f32
        } else if kind == LaneKind::Street && ty.def.mass > 0.0 && ty.def.mass <= 0.3 {
            // a bicycle (stock ones weigh exactly 0.3 t): 15-21 km/h, the `vmax` range their
            // script cuts the drive at (#327)
            15.0 + (seed % 7) as f32
        } else {
            100.0
        };
        // lorries and vans take bends more gently than cars
        state.lat_accel = if kind == LaneKind::Air {
            50.0
        } else if heavy {
            1.6
        } else {
            2.4 + (seed % 7) as f32 * 0.1
        };
        if bus.is_some() {
            // it brakes for its stops the way the town's drivers brake for a light: with
            // 1.5 m/s² of "comfortable" braking the planner braked at half that and crept
            // up to every stop for a hundred metres
            state.decel = 2.1;
            state.accel = state.accel.max(1.0);
            state.min_gap = state.min_gap.max(2.2);
        }
        let (front, rear, half_width) = extents(&ty, if bus.is_some() { 12.0 } else { 4.5 });
        state.front = front;
        state.rear = rear;
        state.length = front + rear;
        if let Some(b) = &bus {
            state.set_route(&self.net, b.route.clone(), s);
        }
        state.speed = (self.net.lanes[lane]
            .speed_limit_kmh
            .min(state.max_speed_kmh)
            / 3.6
            * 0.7)
            .min(state.curve_speed(&self.net));
        if kind == LaneKind::Air {
            state.speed = self.net.lanes[lane]
                .speed_limit_kmh
                .min(state.max_speed_kmh)
                / 3.6;
            state.accel = 0.5;
            state.decel = 0.5;
        }
        // a bus put out at a stop stands there (in the bay, if it has one)
        let at_stop = bus
            .as_ref()
            .and_then(|b| b.stops.first())
            .filter(|st| st.ri == 0 && (st.s - s).abs() < 1.5);
        if let Some(st) = at_stop {
            state.speed = 0.0;
            if st.bay.abs() > 0.01 {
                state.lateral = st.bay;
                state.lateral_target = st.bay;
                state.lateral_ramp = (st.bay, st.bay, 0.0, 1.0);
            }
        }
        let body = place_body(&self.net, &state, &mut vehicle, motion_kind(kind));
        if omsi_cfg::env::var_os("OMSI_DEBUG_TRAFFIC").is_some() {
            let pos = vehicle.position;
            log::info!(
                "spawn {} on lane {lane} s={s:.1} at ({:.1}, {:.1}, {:.1}) heading {:.0}",
                ty.def.path.display(),
                pos.x,
                pos.y,
                pos.z,
                vehicle.heading
            );
        }
        let pass_room = if kind == LaneKind::Street {
            self.pull_out_room(&ty, front, rear, half_width)
        } else {
            0.0
        };
        let id = id.unwrap_or_else(|| {
            let id = self.next_id;
            self.next_id += 1;
            id
        });
        if let Some(v) = speed {
            state.speed = v.min(state.speed.max(v * 0.5));
        }
        if self.debug_population && kind == LaneKind::Street {
            let v = self.viewer;
            let pos = vehicle.position;
            if !self.initial && v.map(|v| v.frames(pos, 2.5)).unwrap_or(false) {
                self.framed_spawns.push((id, pos));
            }
            log::info!("population t={:.1}: car {id} appears at ({:.0}, {:.0}), {:.0} m from the centre, {:.0} m from the camera, in frame {}, behind a building {}{}", self.time, pos.x, pos.y, (pos - center).length(), v.map(|v| (pos - v.pos).length()).unwrap_or(0.0), v.map(|v| v.frames(pos, 2.5)).unwrap_or(false), v.map(|v| self.occluded(world, &v, pos, 2.5)).unwrap_or(false), if self.initial { " (initial)" } else { "" });
        }
        self.cars.push(AiCar {
            id,
            state,
            vehicle,
            render,
            trailer_renders,
            body,
            stopped: 0.0,
            lead_car: None,
            ignore_lead: None,
            crawl: 0.0,
            progress: (0.0, 0.0),
            bus: bus.map(|b| Box::new(BusService::new(b.stops))),
            sounds: None,
            half_width,
            yielding: false,
            light_hold: false,
            reserved: Vec::new(),
            amber: None,
            passing: None,
            gone: false,
            fresh: 1.5,
            merge_after: None,
            holding: None,
            why: ("", 0.0),
            held: false,
            geo_block: None,
            lead_info: None,
            junction_why: String::new(),
            wait_at: None,
            squeeze: None,
            pass_room,
            pass_retry: 0.0,
            light_at: None,
            pull_out: 0.0,
            rail_trail: Default::default(),
            consist_reversed: false,
            park: None,
            seed,
            scheme,
        });
        id
    }

    /// The vehicle/paint sets the random traffic draws from.
    pub fn random_sets(&self) -> Vec<(Arc<VehicleType>, Option<usize>)> {
        let mut sets: Vec<(Arc<VehicleType>, Option<usize>)> = Vec::new();
        for (ty, ..) in &self.types {
            let n = ty.paint_schemes.len().min(AI_SCHEMES);
            let schemes: Vec<Option<usize>> = if n == 0 { vec![None] } else { (0..n).map(Some).collect() };
            for scheme in schemes {
                if !sets.iter().any(|(t, s)| t.def.path == ty.def.path && *s == scheme) {
                    sets.push((ty.clone(), scheme));
                }
            }
        }
        sets
    }

    pub fn precache_random(&mut self, world: &World, renderer: &Renderer, scene: &mut Scene) {
        let t0 = std::time::Instant::now();
        let sets = self.random_sets();
        for chunk in sets.chunks(3) {
            world.prefetch_vehicle_sets(renderer, chunk);
            for (ty, scheme) in chunk {
                world.precache_vehicle(renderer, scene, ty, *scheme);
            }
        }
        world.forget_prefetched();
        for (ty, _) in &sets {
            self.prime_pull_out_room(ty, false);
        }
        log::info!("traffic: {} vehicle/paint sets of the random traffic read and uploaded in {:.1} s", sets.len(), t0.elapsed().as_secs_f32());
    }

    /// Put a timetable bus on the road: an AI car like any other (`create_car`), on its
    /// trip's route at `s` metres into the first lane, with its service (stops).
    /// Returns the car index. `scheme`: the paint scheme to use (Some), or a random one.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_bus(
        &mut self,
        world: &World,
        renderer: &Renderer,
        scene: &mut Scene,
        ty: Arc<VehicleType>,
        route: Vec<usize>,
        s: f32,
        stops: Vec<(usize, f32, f32, f64, i64, f32)>,
        number: Option<(String, String)>,
        hof: Option<Arc<omsi_vehicle::Hof>>,
        scheme: Option<Option<usize>>,
    ) -> Option<usize> {
        let &lane = route.first()?;
        let kind = self.net.lanes.get(lane)?.kind;
        // the options' [AIMaxCountScheduled]: no more timetable vehicles than that at once
        if self.max_scheduled > 0 && self.cars.iter().filter(|c| c.is_bus() || !c.state.route.is_empty()).count() >= self.max_scheduled as usize {
            return None;
        }
        let seed = self.rand();
        let setup = BusSetup {
            route,
            stops: stops.into_iter().map(crate::bus_service::Stop::from_tuple).collect(),
            number,
            hof,
        };
        let center = self.viewer.map(|v| v.pos).unwrap_or_default();
        let id = self.create_car(world, renderer, scene, center, kind, lane, s, ty.clone(), seed, scheme, None, None, Some(setup));
        let ci = self.cars.iter().rposition(|c| c.id == id)?;
        if kind == LaneKind::Air {
            let p = self.cars[ci].vehicle.position;
            let ground = world
                .ground_height(p.x, p.y)
                .map(|g| format!("{:.0} m above the ground", p.z - g))
                .unwrap_or_else(|| "over unloaded ground".into());
            log::info!("aircraft {} on its flight path at ({:.0}, {:.0}), height {:.0} m, {ground}, {:.0} km/h", ty.def.path.file_name().unwrap_or_default().to_string_lossy(), p.x, p.y, p.z, self.cars[ci].state.speed * 3.6);
        }
        Some(ci)
    }

    /// Nearest vehicle ahead of position `s` on `lane` (following the lanes `plan` has
    /// chosen after it, else the first `next`, for up to `look` m): (distance from `s` to
    /// its rear, its speed along the lane, its index). A vehicle beside the lane's middle
    /// far enough to be passed (a bus in its bay) does not count; one coming the other way
    /// round an obstacle does, standing.
    fn obstacle_from(
        &self,
        i: usize,
        lane: usize,
        s: f32,
        plan: Option<&[usize]>,
        look: f32,
        by_lane: &HashMap<usize, Vec<(usize, f32, f32, bool)>>,
    ) -> Option<(f32, f32, usize)> {
        let me = &self.cars[i];
        let mut best: Option<(f32, f32, usize)> = None;
        let mut lane = lane;
        let mut offset = 0.0f32;
        let mut s_from = s;
        let mut upcoming = plan.map(|p| p.iter().copied());
        // (as many lanes as fit into `look`: a junction is a string of short ones, and four
        // of them hid a bus standing just behind it)
        for _ in 0..10 {
            if let Some(list) = by_lane.get(&lane) {
                for &(j, os, lat, foreign) in list {
                    if j == i || os <= s_from {
                        continue;
                    }
                    let o = &self.cars[j];
                    // Two that overlap (a car that ended up beside or in a bus) each found the
                    // other ahead - one's front past the other's rear - and each waited for the
                    // other for good. One that follows this car already and whose middle is
                    // behind this one's is not its lead: the front one drives off.
                    if o.lead_car == Some(me.id) {
                        let h = me.vehicle.heading.to_radians();
                        let fwd = DVec2::new(h.sin(), h.cos());
                        if (o.vehicle.position - me.vehicle.position).truncate().dot(fwd) < 0.0 {
                            continue;
                        }
                    }
                    // (the lateral place this car will have when it gets there: pulling out
                    // round a standing bus, it is clear of it before it arrives)
                    let mine = me.state.lateral_ahead(offset + (os - s_from));
                    if !foreign && (lat - mine).abs() > me.half_width + o.half_width + 0.3 {
                        continue;
                    }
                    let (d, v) = if foreign {
                        (offset + (os - s_from) - o.state.front, 0.0)
                    } else {
                        (offset + (os - s_from) - o.state.rear, o.state.speed)
                    };
                    if best.map(|b| d < b.0).unwrap_or(true) {
                        best = Some((d.max(0.0), v, j));
                    }
                }
            }
            let l = &self.net.lanes[lane];
            offset += l.length() - s_from;
            if offset > look || best.is_some() {
                break;
            }
            let next = match upcoming.as_mut() {
                Some(u) => u.next(),
                None => l.next.first().copied(),
            };
            match next {
                Some(n) => {
                    lane = n;
                    s_from = 0.0;
                }
                None => break,
            }
        }
        best.filter(|d| d.0 < look)
    }

    /// The vehicle car `i` follows: (gap from its front bumper, its speed, its index) along
    /// its lane chain (up to `look` m); during a lane change the target lane counts too.
    fn obstacle_ahead(
        &self,
        i: usize,
        look: f32,
        by_lane: &HashMap<usize, Vec<(usize, f32, f32, bool)>>,
    ) -> Option<(Lead, usize)> {
        let me = &self.cars[i].state;
        // once well over into the new lane, what stands in the old one no longer matters
        // (that is the whole point of pulling out round it)
        let committed = me
            .change
            .map(|c| c.t > 0.4 || (c.bypass && c.wait <= 0.0))
            .unwrap_or(false);
        let plan: Vec<usize> = me.upcoming().collect();
        let mut best = if committed {
            None
        } else {
            self.obstacle_from(i, me.lane, me.s, Some(&plan), look, by_lane)
        };
        if let Some(c) = me.change {
            // along the way it has chosen from the new lane
            if let Some(o) =
                self.obstacle_from(i, c.to, c.s_to, Some(&me.change_plan), look, by_lane)
            {
                if best.map(|b| o.0 < b.0).unwrap_or(true) {
                    best = Some(o);
                }
            }
        }
        // Merging: a car on another lane that leads into the same lane as the next one of
        // ours, and is nearer to that joint, goes first; this car keeps behind it as if it
        // were already ahead in its own lane. (A left turn and the straight lane beside it
        // end in the same exit; taking the turn at a sensible speed, a car used to be run
        // through by the one going straight.)
        if !committed {
            let mut before = self.net.lanes[me.lane].length() - me.s;
            let mut from = me.lane;
            for next in me.upcoming().take(2) {
                if before > look {
                    break;
                }
                for &f in self.net.prev.get(next).map(|v| v.as_slice()).unwrap_or(&[]) {
                    // (two paths of one junction meeting: `junction_stop` sorts that out)
                    if f == from
                        || self.net.crossings[from]
                            .iter()
                            .any(|c| c.other == f && c.merge)
                    {
                        continue;
                    }
                    if before - me.front < 0.5 {
                        continue; // this car is at the joint already
                    }
                    for &(j, os, _, foreign) in by_lane.get(&f).map(|v| v.as_slice()).unwrap_or(&[])
                    {
                        let other = &self.cars[j];
                        if j == i
                            || foreign
                            || other.state.lane != f
                            || other.state.planned_next != Some(next)
                        {
                            continue;
                        }
                        let theirs = self.net.lanes[f].length() - os;
                        if theirs < -2.0 {
                            continue;
                        }
                        // who reaches the joint first goes first; a near tie goes to the one
                        // already let in (the order is kept, it does not flip frame by frame)
                        let t_me = (before - me.front).max(0.0) / me.speed.max(1.0);
                        let dist_them = (theirs - other.state.front).max(0.0);
                        let t_them = if other.yielding || other.light_hold {
                            f32::MAX
                        } else if other.state.speed < 0.5 {
                            time_to(dist_them, 0.0, other.state.accel) + other.state.reaction
                        } else {
                            dist_them / other.state.speed
                        };
                        let kept = self.cars[i].merge_after == Some(other.id);
                        let first = t_them < t_me - 0.4
                            || (kept && t_them < t_me + 1.0)
                            || ((t_them - t_me).abs() <= 0.4
                                && !kept
                                && other.merge_after != Some(self.cars[i].id)
                                && j < i);
                        if first {
                            // behind it at the joint; while it is not past yet, wait at the
                            // joint itself rather than behind a car that is still beside
                            let d = (before - theirs) - other.state.rear;
                            let (d, v) = if d >= 0.0 {
                                (d, other.state.speed)
                            } else {
                                ((before - 1.0).max(0.0), 0.0)
                            };
                            if best.map(|b| d < b.0).unwrap_or(true) {
                                best = Some((d, v, j));
                            }
                        }
                    }
                }
                before += self.net.lanes[next].length();
                from = next;
            }
        }
        best.map(|(d, v, j)| {
            (
                Lead {
                    gap: d - me.front,
                    speed: v,
                    acc: if v > 0.1 { self.cars[j].state.acc } else { 0.0 },
                },
                j,
            )
        })
    }

    /// Is the stretch `s - back .. s + ahead` of `lane` free of cars (other than `i`)?
    fn lane_clear(
        &self,
        i: usize,
        lane: usize,
        s: f32,
        back: f32,
        ahead: f32,
        by_lane: &HashMap<usize, Vec<(usize, f32, f32, bool)>>,
    ) -> bool {
        let Some(list) = by_lane.get(&lane) else {
            return true;
        };
        !list
            .iter()
            .any(|&(j, os, _, _)| j != i && os > s - back && os < s + ahead)
    }

    /// May car `i` move over into `lane` at `s` now? Nothing may be beside it or just ahead,
    /// and every car coming up behind must be able to stop comfortably behind it.
    fn can_merge(
        &self,
        i: usize,
        lane: usize,
        s: f32,
        by_lane: &HashMap<usize, Vec<(usize, f32, f32, bool)>>,
    ) -> bool {
        let me = &self.cars[i].state;
        // the cars on the lane, and those about to come onto it from the lanes before it (a
        // bus changing lanes just after a joint cut in front of a car still on the lane
        // before, which the target lane alone did not show)
        let on = by_lane
            .get(&lane)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
            .iter()
            .copied();
        let before = self
            .net
            .prev
            .get(lane)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
            .iter()
            .flat_map(|&p| {
                let len = self.net.lanes[p].length();
                let single = self.net.lanes[p].next.len() == 1;
                by_lane
                    .get(&p)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[])
                    .iter()
                    .filter(move |e| {
                        !e.3 && (single || self.cars[e.0].state.planned_next == Some(lane))
                            && e.1 > len - 80.0
                    })
                    .map(move |&(j, os, lat, f)| (j, os - len, lat, f))
            });
        on.chain(before).all(|(j, os, _, foreign)| {
            if j == i {
                return true;
            }
            if foreign {
                return false;
            }
            let o = &self.cars[j].state;
            if os >= s {
                // ahead: room for this car, and it must not be much slower
                let gap = os - s - o.rear - me.front;
                gap > 2.0 + (me.speed - o.speed).max(0.0) * 1.5
            } else {
                // behind and standing for this car already (it keeps behind it): it lets it
                // in. Counted as in the way, the bus waiting at the end of its lane to move
                // over and the car stopped behind it for that bus waited on each other for
                // good, and the street behind with them (Spandau's Klosterstrasse).
                if o.speed < 0.3 && self.cars[j].lead_info.is_some_and(|(id, _)| id == self.cars[i].id) {
                    return true;
                }
                // behind: the other driver keeps a time gap and brakes gently
                let gap = s - os - me.rear - o.front;
                gap > 2.0
                    + o.speed * 0.8
                    + (o.speed - me.speed).max(0.0).powi(2) / (2.0 * o.decel.max(1.0))
            }
        })
    }

    /// A timetable vehicle whose route moves over to the next lane: signal, and move as soon
    /// as the lane is free; until then wait before the end of this one. Returns where to stop.
    fn plan_route_change(
        &mut self,
        i: usize,
        by_lane: &HashMap<usize, Vec<(usize, f32, f32, bool)>>,
    ) -> Option<f32> {
        let st = &self.cars[i].state;
        let (to, dir) = st.route_change_due(&self.net)?;
        let len = self.net.lanes[st.lane].length();
        let s_to = self.net.beside_s(st.lane, to, st.s);
        if self.can_merge(i, to, s_to, by_lane) {
            let net = &self.net;
            self.cars[i].state.start_route_change(net, to, dir);
            return None;
        }
        let st = &mut self.cars[i].state;
        st.signal = dir;
        st.signal_time = 1.0;
        Some(len - st.s - 1.0)
    }

    /// Is what car `i` has stopped behind going to stand there for a while (a parked car, a
    /// bus serving its stop, a broken-down or abandoned car, the player's bus waiting)?
    fn standing_obstacle(
        &self,
        i: usize,
        lead: Option<(Lead, Option<usize>)>,
        parked_ahead: bool,
        player_standing: f32,
    ) -> bool {
        let Some((l, who)) = lead else { return false };
        if l.speed.abs() > 0.3 {
            return false;
        }
        match who {
            Some(j) if j < self.cars.len() => {
                let o = &self.cars[j];
                // a bus at its stop, or a car that has stood for long with nothing holding
                // it (not the head of a queue that waits for a light, a junction or a car),
                // or the end of a queue standing behind a bus at its stop
                o.standing_for(self.day_time) > 4.0
                    || (o.stopped > 25.0 && !o.held && self.cars[i].stopped > 6.0)
                    || (o.stopped > 3.0 && self.standing_queue(j).1)
            }
            Some(_) => player_standing > 10.0,
            None => parked_ahead,
        }
    }

    /// The vehicles standing nose to tail from car `j` on (as far as their last steps
    /// show): the length of road they fill (m), and whether a bus serving its stop heads
    /// it - a queue that will not move for a while, which the cars behind may pass as a
    /// whole. (Behind a bus on its layover the whole street
    /// used to wait, five buses and a dozen cars for a quarter of an hour.)
    fn standing_queue(&self, j: usize) -> (f32, bool) {
        let mut k = j;
        let mut len = self.cars[j].state.front + self.cars[j].state.rear;
        let mut long = self.cars[j].standing_for(self.day_time) > 4.0;
        for _ in 0..8 {
            if long {
                break;
            }
            let Some((id, gap)) = self.cars[k].lead_info else {
                break;
            };
            let Some(&n) = self.index_of.get(&id) else {
                break;
            };
            let o = &self.cars[n];
            if gap > 8.0 || o.state.speed > 0.3 || o.light_hold || o.yielding {
                break;
            }
            len += gap.max(0.0) + o.state.front + o.state.rear;
            long = o.standing_for(self.day_time) > 4.0;
            k = n;
        }
        (len, long)
    }

    /// Pull out round something that has stopped in front (a bus at its stop, a car that
    /// gave up, the player standing in the lane): a car held for a few seconds behind a
    /// standing obstacle within 25 m moves to a free neighbouring lane - left first, then
    /// right. With nowhere to go it waits, like everybody else in a jam.
    fn plan_bypass(
        &mut self,
        i: usize,
        gap: Option<f32>,
        standing: bool,
        by_lane: &HashMap<usize, Vec<(usize, f32, f32, bool)>>,
    ) {
        let st = &self.cars[i].state;
        let stuck = self.cars[i].stopped;
        if stuck > 20.0 && standing && omsi_cfg::env::var_os("OMSI_DEBUG_STUCK").is_some() && (self.time * 0.2).fract() < 0.01 {
            let lane = &self.net.lanes[st.lane];
            log::info!("t={:.1}: car {} behind an obstacle {:.1} m for {stuck:.0} s: change {:?} route {} cooldown {:.1} light {} yielding {} left {:?} right {:?} left clear {:?}", self.time, self.cars[i].id, gap.unwrap_or(-1.0), st.change.map(|c| (c.to, c.t, c.wait, c.bypass, c.length)), st.route.len(), st.change_cooldown, self.cars[i].light_hold, self.cars[i].yielding, lane.left, lane.right, lane.left.map(|l| { let s_side = st.s / lane.length().max(1.0) * self.net.lanes[l].length(); (self.open_to(i, l), self.lane_clear(i, l, s_side, 12.0, 30.0, by_lane), self.can_merge(i, l, s_side, by_lane)) }));
        }
        let st = &self.cars[i].state;
        if st.change.is_some()
            || !st.route.is_empty()
            || st.change_cooldown > 0.0
            || stuck < 4.0
            || !standing
            || self.cars[i].light_hold
            || self.cars[i].yielding
        {
            return;
        }
        let Some(d) = gap else { return };
        if d > 25.0 {
            return;
        }
        let lane = &self.net.lanes[st.lane];
        // (a move not over by the end of the lane carries on across the joint, `AiState::drive`:
        // it used to be started only 20 m and more before a lane's end, with the lane beside
        // running on for 30 m by itself - on a road built of 35-40 m spline pieces, Spandau's
        // Falkenseer Chaussee with its kerb lanes lined with parked cars, a car that stopped
        // behind a parked car just past a joint stood there for good with the traffic queued
        // up behind it, the lane beside free)
        if lane.length() - st.s < 9.0 && st.planned_next.is_none() {
            return;
        }
        let frac = st.s / lane.length().max(1.0);
        let (lane_idx, planned) = (st.lane, st.planned_next);
        let turn = planned.map(|n| self.net.lanes[n].turn).unwrap_or(0);
        for (side, dir) in [(lane.left, 1), (lane.right, 2)] {
            let Some(side) = side.filter(|&l| self.open_to(i, l)) else {
                continue;
            };
            // never leave a turn lane just before the junction
            if lane.length() - st.s < 150.0 && turn != 0 && dir != turn {
                continue;
            }
            let side_lane = &self.net.lanes[side];
            let s_side = frac * side_lane.length();
            // the lane beside runs on for 30 m (through its joint)
            let side_on = side_lane.length() - s_side + side_lane.next.iter().map(|&n| self.net.lanes[n].length()).fold(0.0, f32::max);
            if side_on > 30.0
                && self.lane_clear(i, side, s_side, 12.0, 30.0, by_lane)
                && self.can_merge(i, side, s_side, by_lane)
            {
                let net = &self.net;
                self.cars[i].state.start_bypass(net, side, dir);
                self.cars[i].stopped = 0.0;
                if omsi_cfg::env::var_os("OMSI_DEBUG_TRAFFIC").is_some() {
                    log::info!("t={:.1}: car {} pulls out round an obstacle {d:.0} m ahead after {stuck:.0} s: lane {} -> {}", self.time, self.cars[i].id, lane_idx, side);
                }
                return;
            }
        }
    }

    /// On a road with one lane each way: pull out onto the other half round something
    /// standing in the lane, when the oncoming traffic leaves time enough and the car's own
    /// steering gets its front corner past the obstacle's (`AiBody::sweep_clearance`).
    /// `lead` is what it stands behind (`Some(usize::MAX)` the player's bus, `None` a
    /// parked car at `parked_box`).
    #[allow(clippy::too_many_arguments)]
    fn plan_pass(
        &mut self,
        i: usize,
        lead: Option<(Lead, Option<usize>)>,
        obstacle_len: f32,
        standing: bool,
        parked: bool,
        way: &[(usize, f32)],
        by_lane: &HashMap<usize, Vec<(usize, f32, f32, bool)>>,
        player: Option<PlayerBox>,
        parked_box: Option<Obb>,
        feet: &[Footprint],
    ) {
        let car = &self.cars[i];
        let st = &car.state;
        // a parked car is known from afar: the driver pulls out while still rolling up to
        // it; anything else is waited behind for a moment first
        let rolling = parked && st.speed > 0.5;
        if car.passing.is_some()
            || st.change.is_some()
            || (car.stopped < 3.0 && !rolling)
            || !standing
            || car.yielding
            || car.at_stop()
            || self.time < car.pass_retry
        {
            return;
        }
        let Some((lead, who)) = lead else { return };
        let gap = lead.gap;
        // (`OMSI_DEBUG_PASS`: why a car standing behind something does not go round it,
        // twice a second)
        let debug_pass = omsi_cfg::env::var_os("OMSI_DEBUG_PASS").is_some()
            && (self.time * 2.0).floor() != ((self.time - self.last_dt) * 2.0).floor();
        let (id_dbg, t_dbg) = (car.id, self.time);
        let skip = |why: String| {
            if debug_pass {
                log::info!("t={t_dbg:.1}: car {id_dbg} does not pass: {why}");
            }
        };
        // a timetable bus does not pass what stands at its own next stop: it queues for it
        if let Some((ri, ss)) = car.next_stop() {
            if ri >= st.route_index
                && st.route_distance(&self.net, ri, ss) < gap + obstacle_len + st.front + 15.0
            {
                return;
            }
        }
        // the distance from the front bumper to the obstacle's body (the player's box is seen
        // a little long, a parked car's gap two metres short)
        let real = match who {
            Some(usize::MAX) => gap + PLAYER_BOX_MARGIN,
            None => gap + 2.0,
            _ => gap,
        };
        let reach = if rolling {
            (st.speed * st.speed / (2.0 * st.decel) + 12.0).clamp(15.0, 40.0)
        } else {
            22.0 + (car.pass_room - 4.0).max(0.0)
        };
        // (a car still half out from a pass it gave up may start again from there)
        // (lateral towards the oncoming side: a car still half out, or pulled the other way)
        let outward = st.lateral * self.net.oncoming_sign();
        if real > reach || real < 0.3 || outward < -0.5 || outward > 1.6 {
            skip(format!(
                "gap {real:.2} (reach {reach:.1}), lateral {:.2}",
                st.lateral
            ));
            return;
        }
        let lane = &self.net.lanes[st.lane];
        if lane.left.is_some() || lane.right.is_some() {
            return; // a road with lanes to change to: `plan_bypass`
        }
        // the manoeuvre happens before the next junction, or reaches only its mouth
        let pass_len = real + obstacle_len + st.front + st.rear + 6.0;
        // and something that stands just before a light or a junction is waiting there
        let waits_there = way.iter().skip(1).any(|w| {
            (self.net.lanes[w.0].traffic_light.is_some() || !self.net.crossings[w.0].is_empty())
                && w.1 < real + st.front + obstacle_len + 25.0
        });
        if waits_there && !parked {
            skip("it waits before a light or a junction".into());
            return;
        }
        // nor where the road ends (the queue at the end of the network only waits to be taken
        // away: a car that went round it drove into its head)
        if let Some(&(last, d)) = way.last() {
            if self.net.lanes[last].next.is_empty()
                && d + self.net.lanes[last].length() < pass_len + real + 30.0
            {
                skip("the road ends".into());
                return;
            }
        }
        let junction = way
            .iter()
            .skip(1)
            .find(|w| !self.net.crossings[w.0].is_empty())
            .map(|w| w.1)
            .unwrap_or(f32::MAX);
        let open_road: f32 = way
            .iter()
            .take_while(|w| self.net.crossings[w.0].is_empty())
            .map(|w| w.1 + self.net.lanes[w.0].length())
            .fold(0.0, f32::max);
        if open_road.min(junction) < pass_len - if parked { 6.0 } else { -8.0 } {
            skip(format!(
                "a junction in {:.0} m, the pass takes {pass_len:.0} m",
                open_road.min(junction)
            ));
            return;
        }
        let Some((opp, os, side)) = self.net.opposite(st.lane, st.s) else {
            skip("no oncoming lane".into());
            return;
        };
        if !(2.3..=5.5).contains(&side) {
            skip(format!("the oncoming lane is {side:.1} m over"));
            return;
        }
        // back in: 2 m past the obstacle
        let until_d = real + obstacle_len + st.front + st.rear + 2.0;
        // Room to get back in, and to stop there if need be: nothing standing (or stopping)
        // just past the obstacle, no red light close after it. (A car that had passed at speed
        // came back in behind one braking for a red light and braked at 4.5 m/s².)
        let merge_at = st.front + real + obstacle_len;
        let v_cap =
            ((lane.speed_limit_kmh * st.desire).min(st.max_speed_kmh) / 3.6).clamp(4.0, 14.0);
        let v_back = (st.speed * st.speed + 2.0 * st.accel * 0.85 * until_d)
            .sqrt()
            .min(v_cap);
        // (the shortest S-curve back in: three and a half times the offset)
        let back_min = back_in_ramp(side, 0.0, BACK_IN_LAT_ACCEL);
        let need_room =
            (st.length + 4.0 + v_back * v_back / (2.0 * st.decel.max(1.0))).max(back_min + 2.0);
        let mut merge_room = f32::MAX;
        if let Some(l) = car.light_at.filter(|_| car.light_hold) {
            merge_room = l - merge_at - 0.6;
        }
        if let Some(k) = way.iter().rposition(|w| w.1 <= merge_at) {
            let (l, d) = way[k];
            let rest: Vec<usize> = way.iter().skip(k + 1).map(|w| w.0).collect();
            if let Some((ahead, v, j)) = self.obstacle_from(
                i,
                l,
                merge_at - d,
                Some(rest.as_slice()),
                need_room + 20.0,
                by_lane,
            ) {
                let acc = self.cars[j].state.acc;
                if v < 3.0 {
                    merge_room = merge_room.min(ahead);
                } else if acc < -1.0 || self.cars[j].light_hold {
                    // where it will come to a stop
                    merge_room = merge_room
                        .min(ahead + v * v / (2.0 * (-acc).max(self.cars[j].state.decel.max(1.0))));
                }
            }
        }
        if merge_room < need_room {
            skip(format!(
                "no room to get back in ({merge_room:.1} m, needs {need_room:.1} m)"
            ));
            return;
        }
        // back in along an S-curve as long as the speed it will have there asks for, within
        // the room there is (with something standing ahead the car is slowing down anyway)
        let back = back_in_ramp(side, v_back, st.lat_accel.min(BACK_IN_LAT_ACCEL))
            .min((merge_room - 2.0).max(back_min));
        let probe = Passing {
            lane: opp,
            side,
            until: until_d,
            block: real,
            back,
            aborted: false,
            hold: 0.0,
            creep: !rolling,
        };
        let clear_d = probe.clear_at(car.half_width);
        // Time out there: until the car is back far enough to be out of the oncoming
        // traffic's way. Nobody coming may get to where its front will be by then - on the
        // oncoming lane or on the lanes that feed it, back through the junctions beyond
        // (a car that came through the junction ahead used to meet the passer head-on).
        let t_need = pass_time(
            clear_d,
            if rolling { 0.0 } else { real + CREEP_PAST },
            st,
            v_cap,
        ) + 1.5;
        let from = os - st.front - clear_d - 2.0;
        let to = os + st.rear + 8.0;
        let parked_clear = self
            .parked
            .get(&opp)
            .map(|l| {
                l.iter()
                    .all(|&(ps, lat)| lat.abs() > 1.6 || ps < from || ps > to)
            })
            .unwrap_or(true);
        if !parked_clear {
            skip("a parked car on the oncoming lane".into());
            return;
        }
        if let Some((who_opp, t)) = self.oncoming_block(i, opp, from, to, t_need, true, by_lane) {
            skip(format!("car {who_opp} on the oncoming side is there in {t:.1} s, the pass needs {t_need:.1} s"));
            return;
        }
        // Can it steer out round the corner from where it stands? The body is driven along
        // each S-curve in turn (its own wheelbase, lock and steering rate) against the
        // obstacle's box; the gentlest that clears is taken.
        let obstacles: Vec<Obb> = match who {
            Some(usize::MAX) => player
                .map(|(c, h, hl, hw, _)| {
                    vec![Obb::vehicle(
                        c.truncate(),
                        h,
                        hl as f64,
                        hl as f64,
                        hw as f64,
                    )]
                })
                .unwrap_or_default(),
            Some(j) => feet
                .iter()
                .filter(|f| f.car == j)
                .map(|f| f.obb())
                .collect(),
            None => parked_box.into_iter().collect(),
        };
        let extent = (st.front, st.rear, car.half_width);
        let (v_max, accel) = if rolling {
            (st.speed.max(4.0), 0.3)
        } else {
            (8.0, st.accel.min(PULL_OUT_ACCEL))
        };
        // (after half a minute of waiting a driver squeezes out with less to spare)
        let need = if car.stopped > 30.0 {
            0.05
        } else {
            PULL_OUT_CLEARANCE
        };
        let debug = omsi_cfg::env::var_os("OMSI_DEBUG_TRAFFIC").is_some();
        let mut chosen = None;
        let mut best_seen = f64::MIN;
        let mut tried: Vec<String> = Vec::new();
        for ramp in pull_out_ramps(real, st.front, rolling) {
            let mut hyp = st.clone();
            hyp.lateral_target = side * self.net.oncoming_sign();
            hyp.lateral_ramp = (st.lateral, side * self.net.oncoming_sign(), st.odometer, ramp);
            // the checks that go by the lanes must let it go along that way too
            if let (Some(usize::MAX), Some(p)) = (who, player.as_ref()) {
                if let Some(l) = self.player_on_way(&hyp, car.half_width, p, 0.0) {
                    if debug {
                        tried.push(format!("{ramp:.1}: into the bus box at {:.1}", l.gap));
                    }
                    continue;
                }
            }
            if obstacles.is_empty() {
                // (nothing to measure against: the old rule of thumb)
                if real >= if rolling { 1.5 } else { 3.0 } {
                    chosen = Some(ramp);
                }
                break;
            }
            let c = car.body.sweep_clearance(
                &|d| hyp.way_point(&self.net, d),
                st.speed,
                st.reaction,
                accel,
                v_max,
                real + 6.0,
                extent,
                &obstacles,
            );
            best_seen = best_seen.max(c);
            if debug {
                tried.push(format!("{ramp:.1}: {c:.2}"));
            }
            if c >= need {
                chosen = Some(ramp);
                break;
            }
        }
        let Some(ramp) = chosen else {
            if debug {
                // where the obstacle's box is from the car (right, ahead, turned by)
                let rel = obstacles.first().map(|o| {
                    let h = car.vehicle.heading.to_radians();
                    let d = o.center - car.vehicle.position.truncate();
                    let turned = (o.heading.to_degrees() - car.vehicle.heading + 540.0).rem_euclid(360.0) - 180.0;
                    format!("box {:+.2} m right, {:.2} m ahead, turned {turned:+.1}°, half {:.2} x {:.2}", d.x * h.cos() - d.y * h.sin(), d.x * h.sin() + d.y * h.cos(), o.half.x, o.half.y)
                });
                log::info!("t={:.1}: car {} cannot steer out round the obstacle {real:.2} m ahead (best clearance {best_seen:.2} m, room {:.2} m, oncoming lane {side:.2} m over, lateral {:.2}): {}; S-curves {}", self.time, car.id, car.pass_room, st.lateral, rel.unwrap_or_default(), tried.join(", "));
            }
            self.cars[i].pass_retry = self.time + 1.0;
            return;
        };
        let id = car.id;
        let odo = st.odometer;
        let out = self.net.oncoming_sign();
        let car = &mut self.cars[i];
        car.passing = Some(Passing {
            lane: opp,
            side,
            until: odo + until_d,
            block: odo + real,
            back,
            aborted: false,
            hold: 0.0,
            creep: !rolling,
        });
        car.state.lateral_target = side * out;
        // pull out over what room there is (from a standstill a car turns out steeply)
        let lat0 = car.state.lateral;
        car.state.lateral_ramp = (lat0, side * out, odo, ramp);
        car.stopped = 0.0;
        if self.first_passer.is_none() {
            self.first_passer = Some((id, self.time));
        }
        if debug {
            log::info!("t={:.1}: car {id} passes a standing obstacle {real:.2} m ahead on the oncoming lane {opp} ({side:.1} m to the left, S-curve {ramp:.1} m, {t_need:.1} s out there)", self.time);
        }
        if omsi_cfg::env::var_os("OMSI_DEBUG_PASS").is_some() {
            // what it saw coming on the oncoming side
            let lanes = self.net.upstream(opp, from, 150.0, 48);
            let seen: Vec<String> = lanes
                .iter()
                .flat_map(|&(l, off, _)| {
                    by_lane
                        .get(&l)
                        .into_iter()
                        .flatten()
                        .map(move |e| (l, off, *e))
                })
                .map(|(l, off, (j, sj, _, out))| {
                    format!(
                        "car {} on lane {l} at {:.1} m, {:.1} m/s{}",
                        self.cars[j].id,
                        sj + off,
                        self.cars[j].state.speed,
                        if out { " (out passing)" } else { "" }
                    )
                })
                .collect();
            log::info!(
                "  (its stretch {from:.1}..{to:.1} of lane {opp}; lanes before it {:?}; {:?})",
                lanes.iter().map(|e| (e.0, e.1.round())).collect::<Vec<_>>(),
                seen
            );
        }
    }

    /// Who on the oncoming side comes too soon for a car that will be out on lane `opp`
    /// between distances `from` and `to` (that lane's own) for `t_need` seconds: anyone in
    /// that stretch now, or anyone on the lane or on the lanes that lead into it - back
    /// over joints and through junctions, as far as the fastest of them gets in that time -
    /// whose front can get to `from` sooner. `strict`: a moving car may speed up to the
    /// limit (before starting a pass); otherwise it keeps its speed (while out there). A
    /// car waiting at a red light comes once its light changes, one giving way after its
    /// reaction time. Returns (its id, seconds until it is there).
    #[allow(clippy::too_many_arguments)]
    fn oncoming_block(
        &self,
        i: usize,
        opp: usize,
        from: f32,
        to: f32,
        t_need: f32,
        strict: bool,
        by_lane: &HashMap<usize, Vec<(usize, f32, f32, bool)>>,
    ) -> Option<(u64, f32)> {
        let limit = self.net.lanes[opp].speed_limit_kmh / 3.6;
        let look = (limit.clamp(8.0, 20.0) * (t_need + 1.0) + 20.0).min(300.0);
        let lanes = self.net.upstream(opp, from, look, 48);
        for &(l, off, into) in &lanes {
            let Some(list) = by_lane.get(&l) else {
                continue;
            };
            for &(j, s_j, _, foreign) in list {
                if j == i {
                    continue;
                }
                let o = &self.cars[j];
                let c = s_j + off;
                if foreign {
                    // someone from this side out on this lane round something: it is ahead and
                    // going the same way, and one may follow it out while it keeps going; not
                    // while it is slow or gave up in the stretch
                    let going =
                        o.state.speed > 2.0 && o.passing.map(|p| !p.aborted).unwrap_or(false);
                    if l == opp && c > from - 2.0 && c < to && !going {
                        return Some((o.id, 0.0));
                    }
                    continue;
                }
                // only those whose way leads on towards the stretch (a lane can lead there
                // more than one way)
                if into.is_some() && o.state.lane == l && o.state.change.is_none() {
                    if let Some(p) = o.state.planned_next {
                        if !lanes.iter().any(|e| e.0 == p) {
                            continue;
                        }
                    }
                }
                if c - o.state.rear > to {
                    continue; // past it already
                }
                let front = c + o.state.front;
                if front > from {
                    return Some((o.id, 0.0));
                }
                let dist = from - front;
                if dist > look {
                    continue;
                }
                let v = o.state.speed;
                let v_max = ((self.net.lanes[l]
                    .speed_limit_kmh
                    .max(self.net.lanes[opp].speed_limit_kmh)
                    * o.state.desire)
                    .min(o.state.max_speed_kmh)
                    / 3.6)
                    .max(v);
                let go = |d: f32| {
                    if strict || v < 0.3 {
                        arrival_time(d, v, o.state.accel, v_max)
                    } else {
                        d / v
                    }
                };
                // A red light between the car and the stretch holds it until it changes (one
                // further on does not: the car drives through the stretch first); a car that
                // gets to its light only after it has changed does not stop at all.
                let light = o
                    .light_at
                    .filter(|_| o.light_hold)
                    .map(|l| l - o.state.front)
                    .filter(|&l| l <= dist + 0.5);
                let t = match light {
                    Some(l) => {
                        let change = self.light_wait(j);
                        if go(l.max(0.0)) >= change {
                            go(dist)
                        } else {
                            change
                                + o.state.reaction
                                + arrival_time(dist - l.max(0.0), 0.0, o.state.accel, v_max)
                        }
                    }
                    None if v < 0.3 => o.state.reaction + go(dist),
                    None => go(dist),
                };
                if t < t_need {
                    return Some((o.id, t));
                }
            }
        }
        None
    }

    /// Seconds until the red light car `j` waits for may let it go (0 if none holds it).
    fn light_wait(&self, j: usize) -> f32 {
        let st = &self.cars[j].state;
        let way = self.way_lanes(st, 150.0);
        for (k, &(_, d)) in way.iter().enumerate().skip(1) {
            if d > 150.0 {
                break;
            }
            let Some((c, li)) = self.light_at_entry(&way, k) else {
                continue;
            };
            let Some(ctl) = self.lights.get(c) else {
                continue;
            };
            if !TrafficLightController::allows_go(ctl.state(li)) {
                return ctl.time_to_change(li).unwrap_or(0.0);
            }
        }
        0.0
    }

    /// A car out on the oncoming lane round something: if somebody is coming who will be
    /// where its front is headed before it is back out of their way, it gives up while it
    /// still can - back into its lane, stopping short of what it was going round - and
    /// otherwise finishes, with the oncoming traffic stopping short of where it moves back
    /// in (`Traffic::tick` puts it there on their lane).
    fn guard_pass(&mut self, i: usize, by_lane: &HashMap<usize, Vec<(usize, f32, f32, bool)>>) {
        let car = &self.cars[i];
        let Some(p) = car.passing else { return };
        if p.aborted {
            return;
        }
        let st = &car.state;
        let r = p.clear_at(car.half_width) - st.odometer;
        if r <= 0.0 {
            return;
        }
        let Some((opp, os, _)) = self.net.opposite(st.lane, st.s) else {
            return;
        };
        let lane = &self.net.lanes[st.lane];
        let v_cap =
            ((lane.speed_limit_kmh * st.desire).min(st.max_speed_kmh) / 3.6).clamp(4.0, 14.0);
        let creep = if p.creep {
            (p.block + CREEP_PAST - st.odometer).max(0.0)
        } else {
            0.0
        };
        let t_me = pass_time(r, creep, st, v_cap);
        let from = os - st.front - r - 1.0;
        let to = os + st.rear;
        let Some((who, t)) = self.oncoming_block(i, opp, from, to, t_me + 0.5, false, by_lane)
        else {
            return;
        };
        // still in its lane far enough for the oncoming car to get by, and able to stop
        // before the obstacle (a car of two and a half metres needs that much of its lane)
        let v = st.speed;
        let stop_d = v * v / (2.0 * 3.5);
        let shallow = st.lateral * self.net.oncoming_sign() < p.side - car.half_width - 1.45;
        let abortable = shallow && st.odometer + stop_d + 0.4 < p.block;
        let id = car.id;
        let debug = omsi_cfg::env::var_os("OMSI_DEBUG_TRAFFIC").is_some();
        if !abortable {
            if debug && (self.time * 2.0).floor() != ((self.time - self.last_dt) * 2.0).floor() {
                log::info!("t={:.1}: car {id} is out passing with car {who} coming in {t:.1} s, {r:.1} m to go ({t_me:.1} s): finishing", self.time);
            }
            return;
        }
        let odo = st.odometer;
        let lat = st.lateral;
        let car = &mut self.cars[i];
        let mut p = p;
        p.aborted = true;
        p.until = odo;
        // (where it would have waited anyway, if it can stop there)
        p.hold = (p.block - car.pass_room).max(odo + stop_d);
        car.passing = Some(p);
        car.state.lateral_target = 0.0;
        car.state.lateral_ramp = (lat, 0.0, odo, (p.block - odo - 0.5).clamp(2.0, 8.0));
        if debug {
            log::info!("t={:.1}: car {id} gives up passing: car {who} comes in {t:.1} s, it needed {t_me:.1} s more ({r:.1} m); stops within {stop_d:.1} m", self.time);
        }
    }

    /// Overtaking and keeping right: start a lane change when it is safe.
    fn plan_lane_change(
        &mut self,
        i: usize,
        by_lane: &HashMap<usize, Vec<(usize, f32, f32, bool)>>,
    ) {
        let st = &self.cars[i].state;
        if st.change.is_some()
            || !st.route.is_empty()
            || st.change_cooldown > 0.0
            || st.speed < 4.0
            || self.cars[i].passing.is_some()
        {
            return;
        }
        let lane = &self.net.lanes[st.lane];
        // not shortly before the end of the lane (junctions): the indicator, the move and a
        // little margin must fit into what is left of it
        if lane.length() - st.s < (st.speed * 5.5 + 10.0).max(40.0) {
            return;
        }
        let limit = (lane.speed_limit_kmh * st.desire).min(st.max_speed_kmh) / 3.6;
        let frac = st.s / lane.length().max(1.0);
        let (lane_idx, s, planned) = (st.lane, st.s, st.planned_next);
        // turn lanes: within 150 m of the junction get into the lane for the way out
        // (the crossing `[path]` carries the turn: 1 left, 2 right)
        let to_junction = lane.length() - s;
        let turn = planned.map(|n| self.net.lanes[n].turn).unwrap_or(0);
        if to_junction < 150.0 && turn != 0 {
            let want = if turn == 1 { lane.left } else { lane.right };
            if let Some(side) = want.filter(|&l| self.open_to(i, l)) {
                // only if that lane reaches a way out with the same turn
                let ok = self.net.lanes[side]
                    .next
                    .iter()
                    .any(|&n| self.net.lanes[n].turn == turn);
                let s_side = frac * self.net.lanes[side].length();
                if ok && self.can_merge(i, side, s_side, by_lane) {
                    let net = &self.net;
                    self.cars[i].state.turn_wish = turn;
                    self.cars[i].state.start_change(net, side, turn);
                    if omsi_cfg::env::var_os("OMSI_DEBUG_TRAFFIC").is_some() {
                        log::info!("t={:.1}: car {} takes the {} turn lane: {} -> {} ({:.0} m to the junction)", self.time, self.cars[i].id, if turn == 1 { "left" } else { "right" }, lane_idx, side, to_junction);
                    }
                    return;
                }
            }
        }
        // never wander out of a turn lane shortly before the junction
        if to_junction < 150.0 && turn != 0 {
            return;
        }
        // a slow car ahead and a free lane on the left: overtake
        // (on a left-hand-traffic map passing is on the right and the keeping to the left)
        let lht = self.net.left_hand;
        let (pass_side, keep_side, pass_dir, keep_dir) = if lht { (lane.right, lane.left, 2, 1) } else { (lane.left, lane.right, 1, 2) };
        if let Some(left) = pass_side.filter(|&l| self.open_to(i, l)) {
            let plan: Vec<usize> = self.cars[i].state.upcoming().collect();
            if let Some((d, v, _)) = self.obstacle_from(i, lane_idx, s, Some(&plan), 45.0, by_lane)
            {
                let s_left = frac * self.net.lanes[left].length();
                if v < limit * 0.7
                    && v < st.speed + 1.0
                    && d > 8.0
                    && self.lane_clear(i, left, s_left, 20.0, 50.0, by_lane)
                    && self.can_merge(i, left, s_left, by_lane)
                {
                    let net = &self.net;
                    self.cars[i].state.start_change(net, left, pass_dir);
                    if self.last_overtaker.is_none() {
                        self.last_overtaker = Some((self.cars[i].id, self.time));
                    }
                    if omsi_cfg::env::var_os("OMSI_DEBUG_TRAFFIC").is_some() {
                        log::info!("t={:.1}: car {} overtakes: obstacle {d:.0} m at {:.0} km/h, lane {} -> {} ({} {:?}) at ({:.1}, {:.1})", self.time, self.cars[i].id, v * 3.6, lane_idx, left, self.net.lanes[lane_idx].name, self.net.lanes[lane_idx].key, self.cars[i].vehicle.position.x, self.cars[i].vehicle.position.y);
                    }
                    return;
                }
            }
        }
        // keep right when the right lane is free (not into a parking lane: the outer lanes
        // of Spandau's six-lane roads carry `[rule] trafficdensity 0` and the parked cars)
        if let Some(right) = keep_side.filter(|&l| self.open_to(i, l)) {
            let s_right = frac * self.net.lanes[right].length();
            if self.lane_clear(i, right, s_right, 30.0, 70.0, by_lane)
                && self.can_merge(i, right, s_right, by_lane)
                && self.net.lanes[right].length() - s_right > 40.0
            {
                let net = &self.net;
                self.cars[i].state.start_change(net, right, keep_dir);
                if omsi_cfg::env::var_os("OMSI_DEBUG_TRAFFIC").is_some() {
                    log::info!(
                        "t={:.1}: car {} keeps right: lane {} -> {} at ({:.1}, {:.1})",
                        self.time,
                        self.cars[i].id,
                        lane_idx,
                        right,
                        self.cars[i].vehicle.position.x,
                        self.cars[i].vehicle.position.y
                    );
                }
            }
        }
    }

    /// May random traffic drive on `lane` at all (`[rule] no_cars`, `trafficdensity 0`)?
    /// Whether car `i` may change onto `lane`: open to cars, open to its own traffic group
    /// (`[rule]` densities per group: a path open to bicycles only counted as open to
    /// everybody, and the trucks of Vlietlanden changed onto the cycle paths beside the
    /// road, #327) and to its `[ai_veh_type]` (`Lane::allows`).
    fn open_to(&self, i: usize, lane: usize) -> bool {
        let Some(l) = self.net.lanes.get(lane) else { return false };
        if l.no_cars || l.density <= 0.0 {
            return false;
        }
        let car = &self.cars[i];
        if !l.allows(car.state.veh_type) {
            return false;
        }
        match car.state.traffic_pool.as_ref() {
            Some((pool, defaults)) if !l.group_density.is_empty() => l.pool_density(defaults, *pool) > 0.0,
            _ => true,
        }
    }

    /// The lanes of a car's way with their distance from its origin: the current lane (at
    /// minus `s`) and the plan, up to `within` metres.
    /// Where car `i` has to stop for somebody on foot (the distance of its front from its
    /// origin, as the other stops): anybody standing in the strip it is about to sweep, or
    /// stepping into it by the time the car gets there. Only the zebras and signalled
    /// crossings used to count, and only for people strolling the footpaths, so a car
    /// drove at full speed through a passenger walking off a bus across the road, through
    /// somebody leaving a stop, or through anybody standing in the carriageway. A
    /// timetable bus ignores the people waiting at the kerb for it unless they stand well
    /// inside its path (it pulls up right beside them).
    fn people_stop(&self, i: usize, way: &[(usize, f32)]) -> Option<(f32, DVec2)> {
        if self.people.is_empty() {
            return None;
        }
        let car = &self.cars[i];
        let st = &car.state;
        let v = st.speed.max(0.0);
        // as far as the car needs to stop without a jolt, and never less than a car length
        let reach = (v * v / 5.0 + v + 6.0).clamp(8.0, 45.0);
        let origin = car.vehicle.position.truncate();
        let near: Vec<&(DVec3, DVec2, bool)> = self
            .people
            .iter()
            .filter(|(p, _, _)| (p.truncate() - origin).length() < (st.front + reach) as f64 + 6.0)
            .collect();
        if near.is_empty() {
            return None;
        }
        let from = st.front - 1.0;
        let mut first = true;
        for &(l, dl) in way {
            let lane = &self.net.lanes[l];
            let len = lane.length();
            let mut s = (from - dl).max(0.0);
            while s <= len {
                let d = dl + s;
                if d > st.front + reach {
                    return None;
                }
                let (q, h) = lane.at(s);
                let hr = (h as f64).to_radians();
                let right = DVec2::new(hr.cos(), -hr.sin());
                let fwd = DVec2::new(hr.sin(), hr.cos());
                // the car's own offset from the lane counts on the lane it is on
                let lat = if first { st.lateral as f64 } else { 0.0 };
                let c = q.truncate() + right * lat;
                // when the car's front gets here, at most two seconds on
                let t = (((d - st.front).max(0.0)) / v.max(1.0)).min(2.0) as f64;
                for (p, pv, waiting) in &near {
                    // on the same level only: somebody on a footbridge over the road, in a
                    // subway under it or on a platform above it is in nobody's way here (a
                    // car would stand in front of nothing anybody could see)
                    if (p.z - q.z).abs() > PEOPLE_LEVEL {
                        continue;
                    }
                    let half = if *waiting && car.is_bus() {
                        car.half_width as f64 - 0.3
                    } else {
                        car.half_width as f64 + 0.3
                    };
                    for at in [p.truncate(), p.truncate() + *pv * t] {
                        let rel = at - c;
                        if rel.dot(fwd).abs() <= 0.55 && rel.dot(right).abs() < half {
                            return Some((d - 1.5, p.truncate()));
                        }
                    }
                }
                s += 1.0;
            }
            first = false;
        }
        None
    }

    /// The light that holds a car at the start of `way[k]`: that lane's light, unless the
    /// car has already gone through a light of the same crossing object on the way there
    /// (the lanes before it, back to where it came into that object). A car turning right
    /// went on green and then stopped as it came round the corner, at the light of the
    /// cross traffic on the path its turn joins - a stop line in mid-junction nobody sees.
    fn light_at_entry(&self, way: &[(usize, f32)], k: usize) -> Option<(usize, usize)> {
        let l = way[k].0;
        let light = self.net.lanes[l].traffic_light?;
        let object = |x: usize| {
            let lane = &self.net.lanes[x];
            lane.key.filter(|_| lane.source == 2).map(|key| (key.tile, key.id))
        };
        let here = object(l)?;
        for &(p, _) in way[..k].iter().rev() {
            if object(p) != Some(here) {
                break;
            }
            if self.net.lanes[p].traffic_light.is_some() {
                return None;
            }
        }
        Some(light)
    }

    fn way_lanes(&self, st: &AiState, within: f32) -> Vec<(usize, f32)> {
        let mut out = vec![(st.lane, -st.s)];
        let mut d = self.net.lanes[st.lane].length() - st.s;
        let plan: Vec<usize> = match st.change {
            Some(c) => std::iter::once(c.to)
                .chain(st.change_plan.iter().copied())
                .collect(),
            None => st.upcoming().collect(),
        };
        if let Some(c) = st.change {
            // over on the new lane: its distances count from the same place
            out.clear();
            out.push((c.to, -c.s_to));
            d = self.net.lanes[c.to].length() - c.s_to;
            for &l in plan.iter().skip(1) {
                if d > within {
                    break;
                }
                out.push((l, d));
                d += self.net.lanes[l].length();
            }
            return out;
        }
        for l in plan {
            if d > within {
                break;
            }
            out.push((l, d));
            d += self.net.lanes[l].length();
        }
        out
    }

    /// The junction on car `i`'s way within `within` metres: its lanes that cross or meet
    /// others (or a footpath), and the lane after it.
    fn junction_ahead(&self, way: &[(usize, f32)]) -> Option<Junction> {
        let has = |l: usize| !self.net.crossings[l].is_empty() || !self.net.walks[l].is_empty();
        let object = |l: usize| {
            self.net.lanes[l]
                .key
                .filter(|_| self.net.lanes[l].source == 2)
                .map(|k| (k.tile, k.id))
        };
        let mut j: Option<Junction> = None;
        for (k, &(l, d)) in way.iter().enumerate() {
            match j.as_mut() {
                None => {
                    if has(l) {
                        j = Some(Junction {
                            lanes: vec![(l, d)],
                            exit: None,
                            inside: k == 0,
                        });
                    }
                }
                Some(jn) => {
                    if object(l).is_some() && object(l) == object(jn.lanes[0].0) {
                        jn.lanes.push((l, d));
                    } else {
                        jn.exit = Some((l, d));
                        break;
                    }
                }
            }
        }
        j
    }

    /// The light program's verdict for car `i`: where it has to stop (distance from its
    /// origin), or None. A car that can stop comfortably stops at yellow; one too close
    /// drives on and remembers that it did, so the red that follows does not stop it in the
    /// middle of the junction.
    fn light_stop(&mut self, i: usize, way: &[(usize, f32)]) -> Option<f32> {
        let car = &self.cars[i];
        let st = &car.state;
        let v = st.speed;
        let mut stop = None;
        let mut amber = car.amber;
        for (k, &(_, d)) in way.iter().enumerate().skip(1) {
            if d > 150.0 {
                break;
            }
            let Some((c, li)) = self.light_at_entry(way, k) else {
                continue;
            };
            let Some(ctl) = self.lights.get(c) else {
                continue;
            };
            let gap = d - st.front;
            let comfortable = v * v / (2.0 * st.decel * 1.4) + 1.0;
            let possible = v * v / (2.0 * MAX_BRAKE * 0.8);
            let go = match TrafficLightController::aspect(ctl.state(li)) {
                Aspect::Green | Aspect::Dark => {
                    // at the line when it showed green: that car goes, whatever comes next
                    // (a light that is green for a second a cycle - Westcountry's lights on
                    // its invisible lanes - let nobody through: the first car was still
                    // taking in the green when it went red again, for ever)
                    if gap < 3.0 {
                        amber = Some((c, li));
                    }
                    true
                }
                Aspect::Yellow | Aspect::GreenYellow => {
                    if amber == Some((c, li)) || gap < comfortable {
                        amber = Some((c, li));
                        true
                    } else {
                        false
                    }
                }
                // decided to go on yellow and too close to stop now, or past stopping at all
                Aspect::Red | Aspect::RedYellow => {
                    (amber == Some((c, li)) && gap < comfortable) || gap < possible - 0.5
                }
            };
            if !go {
                stop = Some(d);
                break;
            }
        }
        // the light the car went through on yellow is behind it
        if let Some(a) = amber {
            if !way
                .iter()
                .skip(1)
                .any(|&(l, _)| self.net.lanes[l].traffic_light == Some(a))
            {
                amber = None;
            }
        }
        self.cars[i].amber = amber;
        stop
    }

    /// Right of way at the junction ahead of car `i`: where it has to wait (distance from
    /// its origin), or None when it may go - in which case it claims the junction's lanes.
    /// It gives way to anyone already in the junction on a crossing path, to anyone who
    /// has claimed a crossing path and arrives before it could be through, to traffic with
    /// the right of way that is close enough in time (its `accept_gap`), to pedestrians on
    /// a crossing, and it does not drive into a junction it could not leave (a queue on
    /// the exit). Cars waiting on each other all round are resolved in favour of the one
    /// that has waited longest. A driver who has decided to go keeps to it (the claim
    /// stands) unless someone is actually in the way: weighing the gap again every frame
    /// made two cars take turns at stopping and going, a hard brake every other frame.
    /// `way` is the car's own way: where its own path crosses itself nothing is to be
    /// given way to.
    #[allow(clippy::too_many_arguments)]
    fn junction_stop(
        &mut self,
        i: usize,
        jn: &Junction,
        way: &[(usize, f32)],
        lead: Option<Lead>,
        on_lane: &HashMap<usize, Vec<(usize, f32, f32, bool)>>,
        coming: &HashMap<usize, Vec<(usize, f32)>>,
        reservations: &mut HashMap<usize, Vec<usize>>,
        walkers: &HashMap<usize, Vec<f32>>,
    ) -> Option<f32> {
        let car = &self.cars[i];
        let st = &car.state;
        let v = st.speed;
        let entry = jn.lanes[0].1;
        let decide = (v * v / (2.0 * st.decel) + 12.0).clamp(20.0, 70.0);
        let release = |reservations: &mut HashMap<usize, Vec<usize>>, lanes: &[usize]| {
            for l in lanes {
                if let Some(list) = reservations.get_mut(l) {
                    list.retain(|&c| c != i);
                }
            }
        };
        if !jn.inside && entry - st.front > decide {
            // too far to decide; a claim made just inside that distance stands (slowing
            // down moves the line, and letting go and claiming again in turns made the
            // cross traffic stop and go with it)
            if entry - st.front > decide + 20.0 {
                let old = std::mem::take(&mut self.cars[i].reserved);
                release(reservations, &old);
            }
            return None;
        }
        // queued behind someone who is not through the junction yet: no claim
        let queued = !jn.inside
            && lead
                .map(|l| l.speed < 1.0 && l.gap < entry - st.front + 3.0)
                .unwrap_or(false);
        let a_me = st.accel;
        // decided already (a claim from the frames before), or past the point where it could
        // still stop without an emergency brake
        let committed = car.reserved.contains(&jn.lanes[0].0);
        // (a stop line is kept 0.6 m off)
        let room = entry - st.front - 0.6;
        // (a car creeping up to its line can always stop: at the line the room is nothing,
        // and the car standing there used to count as one that could not stop any more)
        let cannot_stop = !jn.inside && v > 1.0 && room < v * v / (2.0 * MAX_BRAKE * 0.7);
        let cannot_stop_gently = !jn.inside && v > 1.0 && room < v * v / (2.0 * st.decel * 1.5);
        let mut hard = false;
        // what is only a matter of the rules (right of way, a full exit) against someone
        // physically in the way
        let mut ruled = false;
        let mut soft: Vec<usize> = Vec::new();
        let explain = omsi_cfg::env::var_os("OMSI_DEBUG_JUNCTION").is_some() || omsi_cfg::env::var_os("OMSI_DEBUG_STUCK").is_some();
        let mut why: Vec<String> = Vec::new();
        let mut stop_at = if jn.inside { None } else { Some(entry) };
        let me_id = car.id;
        // (Omsi.exe: a vehicle whose script sets `TrafficPriority` claims a crossing with
        // priority 1000, above any vehicle type's, FUN_007d9128 - the AI ambulance as much
        // as the player; ours honoured it for the player's bus only)
        let prio = |c: &AiCar| c.vehicle.var("TrafficPriority").is_some_and(|v| v > 0.5);
        let me_prio = prio(car);
        // A driver who has waited long accepts a shorter gap (the critical gap shrinks with
        // the wait, by up to a third after forty seconds): a bus that needed twelve seconds
        // of a busy main road stood at the mouth of its side road for minutes.
        let wait = self.cars[i].state.yield_time;
        let patience = 1.0 - (wait / 40.0).min(1.0) / 3.0;
        for &(l, dl) in &jn.lanes {
            for c in &self.net.crossings[l] {
                let point = dl + c.at;
                let m = c.other;
                if way.iter().any(|w| w.0 == m) {
                    continue; // its own way
                }
                // Where the bodies meet (`Crossing::before`/`after`): two paths that cross at a
                // shallow angle, or two turns bending towards each other, bring the cars
                // together metres before their centre lines cross (two turning cars used to
                // touch while the one that gave way still rolled towards "its" point). Paths
                // that run into one another (a merge) are ordered at the joint itself.
                if point + c.after < -st.rear - 0.3 {
                    continue; // passed already
                }
                // vehicles on the other lane, or coming to it
                let on = on_lane
                    .get(&m)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[])
                    .iter()
                    .filter(|e| !e.3)
                    .map(|&(j, sj, _, _)| (j, c.other_at - sj, true));
                let near = coming
                    .get(&m)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[])
                    .iter()
                    .map(|&(j, dj)| (j, dj + c.other_at, false));
                for (j, dj, is_on) in on.chain(near) {
                    if j == i {
                        continue;
                    }
                    let o = &self.cars[j];
                    // it waits behind this car's body: it will not come before this one moves
                    if self.geo_prev.get(j).copied().flatten() == Some(me_id) {
                        continue;
                    }
                    if dj + c.other_after < -o.state.rear - 0.3 {
                        continue; // it is through
                    }
                    let t_clear = time_to(point + c.after + st.rear + 0.3, v, a_me)
                        + if v < 0.5 { st.reaction } else { 0.0 };
                    // when this car's front gets to the meeting place
                    let t_mine = time_to(point - c.before - st.front, v, a_me)
                        + if v < 0.1 { st.reaction } else { 0.0 };
                    // A meeting place far into the junction's way (the far side of a
                    // roundabout its path runs round to) is not weighed at the line: the car
                    // goes in behind the traffic already on its way and gives way there if
                    // it has to (inside the junction every meeting place counts). Weighed
                    // at the line, an entry waited for a gap of nine seconds on a ring that
                    // never had one, and the queue behind it stood for minutes.
                    if !jn.inside && point - c.before - st.front > 25.0 {
                        continue;
                    }
                    // A stalled car neither claims an imminent arrival nor accelerates
                    // freely in the arrival prediction, even without a reservation.
                    let stalled = (o.stopped > 4.0 && o.state.speed < 0.1)
                        || o.crawl >= 8.0
                        || (o.state.speed < 1.5
                            && o.lead_info.is_some_and(|(lid, gap)| gap < 8.0 && self.cars.iter().find(|x| x.id == lid).is_some_and(|x| x.state.speed < 1.0)));
                    let claimed = reservations
                        .get(&m)
                        .map(|r| r.contains(&j))
                        .unwrap_or(false)
                        && !stalled;
                    let theirs = dj - c.other_before - o.state.front;
                    // it waits for someone else before this meeting place (a car that gives
                    // way further on still rolls through here on its way to its line)
                    let waits_short = o.light_hold
                        || (o.yielding
                            && !claimed
                            && o.wait_at
                                .map(|w| w - 0.6 <= dj - c.other_before)
                                .unwrap_or(false));
                    // (it arrives in `t_j` seconds)
                    let t_j = crossing_arrival(&o.state, theirs, claimed, waits_short, stalled);
                    if is_on && theirs <= 0.3 {
                        // in the meeting place right now: unless this car is further in
                        // already (then it is the other one that has to wait)
                        let mine_in = st.front - (point - c.before);
                        let theirs_in = -theirs;
                        let ahead = mine_in > 0.0
                            && (mine_in > theirs_in + 0.3
                                || ((mine_in - theirs_in).abs() <= 0.3 && me_id > o.id));
                        if !ahead {
                            hard = true;
                            if explain {
                                why.push(format!("car {} in the crossing of lanes {l}/{m}", o.id));
                            }
                            if jn.inside {
                                stop_at = Some(stop_at.unwrap_or(f32::MAX).min(point - c.before));
                            }
                        }
                        continue;
                    }
                    if claimed || (is_on && o.state.speed > 0.5 && !waits_short) {
                        // Both have decided (or this one is in the junction already): the one
                        // that gets there first goes first, a tie goes to the lower number.
                        // Otherwise two cars standing at their lines, each with a claim,
                        // waited for each other for good.
                        let me_decided = committed || jn.inside;
                        let first = if me_decided {
                            t_j < t_mine - 0.3 || ((t_j - t_mine).abs() <= 0.3 && o.id < me_id)
                        } else {
                            true
                        };
                        if first && t_j < t_clear * if me_decided { 1.0 } else { patience } + 1.0 {
                            hard = true;
                            if explain {
                                why.push(format!("car {} ({}) arrives at {l}/{m} in {t_j:.1} s, this one in {t_mine:.1} s, clear in {t_clear:.1} s [its v {:.2} stood {:.1} crawl {:.1} yielding {} lead {:?} lane {} theirs {:.1}]", o.id, if claimed { "claimed" } else { "on it" }, o.state.speed, o.stopped, o.crawl, o.yielding, o.lead_info, o.state.lane, theirs));
                            }
                            if jn.inside {
                                stop_at = Some(stop_at.unwrap_or(f32::MAX).min(point - c.before));
                            }
                        }
                        continue;
                    }
                    // a vehicle with priority goes before one without, whatever the lanes say;
                    // one without gives way to it
                    let o_prio = prio(o);
                    if jn.inside || committed || (me_prio && !o_prio) || (!self.net.must_yield(l, m) && !(o_prio && !me_prio)) {
                        continue;
                    }
                    // the gap a driver takes in the main road's traffic: the critical gap of
                    // 5 to 7.5 s (by driver), and time enough to be through
                    if t_j < (st.accept_gap + 2.0).max(t_clear + 1.0) * patience {
                        if t_j == f32::MAX || o.state.speed < 0.3 {
                            soft.push(j);
                        } else {
                            ruled = true;
                            if explain {
                                why.push(format!("car {} has the right of way at {l}/{m}, arrives in {t_j:.1} s, clear in {t_clear:.1} s", o.id));
                            }
                        }
                    }
                }
                // The player's bus (and a LAN player's) on that lane or coming to it, by the
                // same rules: before, the cars saw it only as a box in their way a second and
                // a half ahead, and pulled out of a side road in front of a bus coming along
                // the main road, or turned across it.
                for u in &self.way_users {
                    let Some(&(_, dm)) = u.lanes.iter().find(|x| x.0 == m) else { continue };
                    let (verdict, t_j, t_clear) = way_user_verdict(st, u, dm, c, point, jn.inside, committed, me_prio, self.net.must_yield(l, m), patience);
                    match verdict {
                        Verdict::Free => {}
                        Verdict::Hard => {
                            hard = true;
                            if explain {
                                why.push(format!("the player's vehicle in or at the crossing of lanes {l}/{m} (arrives in {t_j:.1} s, this one is clear in {t_clear:.1} s)"));
                            }
                            if jn.inside {
                                stop_at = Some(stop_at.unwrap_or(f32::MAX).min(point - c.before));
                            }
                        }
                        Verdict::Ruled => {
                            ruled = true;
                            if explain {
                                why.push(format!("the player's vehicle has the right of way at {l}/{m}, arrives in {t_j:.1} s, clear in {t_clear:.1} s"));
                            }
                        }
                    }
                }
            }
            // people on a zebra or a signalled crossing
            for &(w, at, w_at) in &self.net.walks[l] {
                let point = dl + at;
                if point < st.front - 1.0 {
                    continue;
                }
                if walkers
                    .get(&w)
                    .map(|ps| ps.iter().any(|&p| (p - w_at).abs() < 3.0))
                    .unwrap_or(false)
                    && point - st.front < 30.0
                {
                    hard = true;
                    if explain {
                        why.push(format!(
                            "someone on the crossing of lane {l} and footpath {w}"
                        ));
                    }
                    let before = point - 2.5;
                    stop_at = Some(stop_at.unwrap_or(before).min(before));
                }
            }
        }
        // keep the junction clear: the exit must take the whole car
        let ruled_before_exit = ruled;
        let mut exit_full = false;
        if !jn.inside {
            if let Some((e, de)) = jn.exit {
                let room = on_lane
                    .get(&e)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[])
                    .iter()
                    .filter(|x| !x.3 && x.0 != i)
                    .map(|&(j, sj, _, _)| (sj - self.cars[j].state.rear, self.cars[j].state.speed))
                    .fold(None::<(f32, f32)>, |acc, x| {
                        if acc.map(|a| x.0 < a.0).unwrap_or(true) {
                            Some(x)
                        } else {
                            acc
                        }
                    });
                if let Some((space, speed)) = room {
                    if speed < 1.5 && space < st.length + st.min_gap && de < 40.0 {
                        ruled = true;
                        exit_full = true;
                        if explain {
                            why.push(format!("exit {e} full ({space:.1} m)"));
                        }
                    }
                }
            }
        }
        let mut blocked =
            (hard && !cannot_stop) || ((ruled || !soft.is_empty()) && !cannot_stop_gently);
        // held only by a full exit for long: a ring of queues each waiting for the next
        // junction's exit (round a block) never clears by itself - squeeze in, as drivers do
        if blocked && exit_full && !hard && !ruled_before_exit && soft.is_empty() && wait > GRIDLOCK_WAIT {
            blocked = false;
            if omsi_cfg::env::var_os("OMSI_DEBUG_TRAFFIC").is_some() {
                log::info!("t={:.1}: car {} squeezes into a full exit after {wait:.0} s (gridlock)", self.time, self.cars[i].id);
            }
        }
        if !hard && !ruled && !soft.is_empty() && wait > 2.5 + st.reaction {
            // everybody is waiting for somebody: the longest waiter goes
            let wins = soft.iter().all(|&j| {
                let o = &self.cars[j];
                (o.yielding || o.state.speed < 0.3)
                    && (wait > o.state.yield_time + 0.05
                        || ((wait - o.state.yield_time).abs() <= 0.05 && self.cars[i].id < o.id))
            });
            if wins {
                blocked = false;
                if omsi_cfg::env::var_os("OMSI_DEBUG_TRAFFIC").is_some() {
                    log::info!("t={:.1}: car {} ends a wait of {wait:.1} s at a junction ({} waiting on it)", self.time, self.cars[i].id, soft.len());
                }
            }
        }
        let lanes: Vec<usize> = jn.lanes.iter().map(|x| x.0).collect();
        // (and every ten seconds of a long wait)
        let long_wait =
            wait > 15.0 && (wait / 10.0).floor() != ((wait - self.last_dt) / 10.0).floor();
        if explain && (blocked != self.cars[i].yielding || (blocked && long_wait)) {
            log::info!("t={:.2}: car {} at {:.1} m/s {} the junction {:?} (entry {:.1} m, inside {}): hard {hard}, waiting for {:?}, claims {:?} {:?}", self.time, self.cars[i].id, v, if blocked { "waits at" } else { "goes into" }, lanes, entry - st.front, jn.inside, soft.iter().map(|&j| self.cars[j].id).collect::<Vec<_>>(), lanes.iter().map(|l| reservations.get(l).map(|r| r.iter().map(|&j| self.cars[j].id).collect::<Vec<_>>())).collect::<Vec<_>>(), why);
        }
        if explain {
            self.cars[i].junction_why = if blocked { format!("{why:?} soft {:?}", soft.iter().map(|&j| self.cars[j].id).collect::<Vec<_>>()) } else { String::new() };
        }
        // A driver who has waited long at the line makes himself seen: he keeps a claim on
        // his way through while still waiting, so the cars not yet committed to the
        // junction hold back for him and he goes once those already on their way are
        // through. Without it a side road's car at a busy main road waited four and a half
        // minutes while every newcomer claimed the junction first. (Two such on crossing
        // ways are sorted out by the claims' order: the first there, a tie the lower number.)
        if blocked && !jn.inside && wait > LONG_WAIT_CLAIM && !queued {
            for &l in &lanes {
                let list = reservations.entry(l).or_default();
                if !list.contains(&i) {
                    list.push(i);
                }
            }
            let car = &mut self.cars[i];
            for &l in &lanes {
                if !car.reserved.contains(&l) {
                    car.reserved.push(l);
                }
            }
            return stop_at;
        }
        if blocked && !jn.inside {
            let old = std::mem::take(&mut self.cars[i].reserved);
            release(reservations, &old);
            return stop_at;
        }
        if blocked {
            return stop_at;
        }
        if queued && !jn.inside {
            let old = std::mem::take(&mut self.cars[i].reserved);
            release(reservations, &old);
            return None;
        }
        // claim the way through
        for &l in &lanes {
            let list = reservations.entry(l).or_default();
            if !list.contains(&i) {
                list.push(i);
            }
        }
        let car = &mut self.cars[i];
        for l in lanes {
            if !car.reserved.contains(&l) {
                car.reserved.push(l);
            }
        }
        None
    }

    /// Two cars that have each other for their lead - a car that ended up in a bus's body,
    /// each finding the other in its way - wait for each other for good. The one further
    /// along its lane (on the same lane; else the lower number) stops taking the other for
    /// its lead for a few seconds and drives off.
    fn break_lead_pairs(&mut self) {
        let index: HashMap<u64, usize> = self.cars.iter().enumerate().map(|(k, c)| (c.id, k)).collect();
        let mut pairs: Vec<(usize, u64)> = Vec::new();
        for (a, c) in self.cars.iter().enumerate() {
            let Some(bid) = c.lead_car else { continue };
            let Some(&b) = index.get(&bid) else { continue };
            if b <= a || self.cars[b].lead_car != Some(c.id) {
                continue;
            }
            let (sa, sb) = (&self.cars[a].state, &self.cars[b].state);
            let a_goes = if sa.lane == sb.lane { sa.s > sb.s } else { c.id < bid };
            let (go, other) = if a_goes { (a, bid) } else { (b, c.id) };
            pairs.push((go, other));
        }
        for (go, other) in pairs {
            if omsi_cfg::env::var_os("OMSI_DEBUG_STUCK").is_some() {
                log::info!("t={:.1}: cars {} and {} each waited for the other: {} drives off", self.time, self.cars[go].id, other, self.cars[go].id);
            }
            self.cars[go].ignore_lead = Some((other, self.time as f64 + 5.0));
        }
    }

    /// The footprints of all AI vehicles, rear sections and trailers included.
    fn footprints(&self) -> Vec<Footprint> {
        let mut out = Vec::with_capacity(self.cars.len() + 8);
        for (i, c) in self.cars.iter().enumerate() {
            let st = &c.state;
            let h = c.vehicle.heading.to_radians();
            let (fwd, right) = (DVec2::new(h.sin(), h.cos()), DVec2::new(h.cos(), -h.sin()));
            let center = c.vehicle.position.truncate() + fwd * ((st.front - st.rear) * 0.5) as f64;
            out.push(Footprint {
                car: i,
                center,
                fwd,
                right,
                half_len: ((st.front + st.rear) * 0.5) as f64,
                half_w: c.half_width as f64,
                speed: st.speed,
                z: c.vehicle.position.z,
            });
            for t in &c.vehicle.trailers {
                if let Some(bb) = t.ty.def.bounding_box {
                    out.push(Footprint::from_obb(
                        i,
                        &omsi_sim::collision::Obb::from_box(bb, t.position, t.heading),
                        st.speed,
                    ));
                }
            }
        }
        out
    }

    /// May a vehicle of `ty` be put on the road at `pos` facing `heading` (deg)? Not onto
    /// (or right up against) another vehicle or the player's: a timetable bus used to be
    /// checked only for a vehicle origin within 9 m, and a layover bus appeared inside the
    /// articulated bus waiting at the same stand.
    pub fn spawn_clear(&self, ty: &VehicleType, pos: DVec3, heading: f64) -> bool {
        let (front, rear, half_w) = extents(ty, 12.0);
        let h = heading.to_radians();
        let (fwd, right) = (DVec2::new(h.sin(), h.cos()), DVec2::new(h.cos(), -h.sin()));
        let me = Footprint {
            car: usize::MAX,
            center: pos.truncate() + fwd * ((front - rear) * 0.5) as f64,
            fwd,
            right,
            half_len: ((front + rear) * 0.5) as f64,
            half_w: half_w as f64,
            speed: 0.0,
            z: pos.z,
        };
        if self.footprints().iter().any(|f| {
            (f.center - me.center).length() < f.half_len + me.half_len + 5.0
                && (f.z - me.z).abs() < 4.0
                && f.overlaps(&me, 1.0)
        }) {
            return false;
        }
        // nor just in front of a car driving up to that place (it would have to stop hard)
        let in_front = self.cars.iter().any(|c| {
            let rel = pos - c.vehicle.position;
            let h = c.vehicle.heading.to_radians();
            let (along, across) = (
                rel.x * h.sin() + rel.y * h.cos(),
                (rel.x * h.cos() - rel.y * h.sin()).abs(),
            );
            let v = c.state.speed;
            along > 0.0
                && along
                    < (c.state.front + rear + 15.0 + v * v / (2.0 * c.state.decel.max(1.0)) * 1.5)
                        as f64
                && across < 3.0
                && (rel.z).abs() < 4.0
        });
        if in_front {
            return false;
        }
        match self.player {
            Some((c, ph, hl, hw, _)) => {
                let h = ph.to_radians();
                let p = Footprint {
                    car: usize::MAX,
                    center: c.truncate(),
                    fwd: DVec2::new(h.sin(), h.cos()),
                    right: DVec2::new(h.cos(), -h.sin()),
                    half_len: hl as f64,
                    half_w: hw as f64,
                    speed: 0.0,
                    z: c.z,
                };
                (p.z - me.z).abs() > 4.0 || !p.overlaps(&me, 1.5)
            }
            None => true,
        }
    }

    /// Another AI vehicle's body in car `i`'s way where the lanes do not show it: a bus
    /// standing in its bay across a turning path, a car stopped half inside a junction, the
    /// rear section of an articulated bus still swinging round, a car cutting in. The way
    /// ahead is swept with the car's width against every footprint near it (the lanes alone
    /// let a car turn right into the side of a bus that stood 1.7 m out in its bay).
    /// Two vehicles that each stand in the other's way are sorted out by `geo_prev`: the one
    /// with the higher id goes, the other waits.
    fn body_in_way(
        &self,
        i: usize,
        feet: &[Footprint],
        by_lane: &HashMap<usize, Vec<(usize, f32, f32, bool)>>,
    ) -> Option<(Lead, usize)> {
        let car = &self.cars[i];
        let st = &car.state;
        if self.net.lanes[st.lane].kind == LaneKind::Air {
            return None;
        }
        let pos = car.vehicle.position.truncate();
        let z = car.vehicle.position.z;
        let look = (st.speed * st.speed / (2.0 * st.decel.max(1.0)) + st.speed * 2.0 + 12.0)
            .clamp(12.0, LOOK_AHEAD);
        let reach = st.front + look;
        // what the car is pulling out round does not stop it
        let rounding: Vec<usize> = if car.passing.map(|p| !p.aborted).unwrap_or(false)
            || st.change.map(|c| c.bypass).unwrap_or(false)
        {
            by_lane
                .get(&st.lane)
                .map(|v| {
                    v.iter()
                        .filter(|e| self.cars[e.0].state.speed < 0.5)
                        .map(|e| e.0)
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let me = car.id;
        let near: Vec<&Footprint> = feet
            .iter()
            .filter(|f| f.car != i && !rounding.contains(&f.car) && (f.z - z).abs() < BODY_LEVEL + 6.0)
            .filter(|f| (f.center - pos).length() < reach as f64 + f.half_len + f.half_w + 2.0)
            .filter(|f| {
                let o = &self.cars[f.car];
                // it waits for this car already: the higher id goes - but never into it: a
                // body within reach of the bumper stops the car whoever waits for whom (a car
                // drove straight into the side of a bus that stood waiting for it)
                !(self.geo_prev.get(f.car).copied().flatten() == Some(me)
                    && me > o.id
                    && !f.overlaps(&car_foot(car, 1.5), 0.0))
            })
            .collect();
        if near.is_empty() {
            return None;
        }
        let hw = car.half_width as f64;
        let mut d = st.front + 0.2;
        let mut p3 = st.way_point(&self.net, d);
        while d <= reach {
            // (finer close by, where the gap matters)
            let step = if d < st.front + 20.0 { 0.75 } else { 1.5 };
            let q3 = st.way_point(&self.net, d + step);
            let (p, q) = (p3.truncate(), q3.truncate());
            let dir = (q - p).normalize_or_zero();
            let across = DVec2::new(dir.y, -dir.x);
            for f in &near {
                // on the level of the way there, not of the car now: by the car's own
                // height a car on a bridge counted as in the way of one on the ramp down to
                // the road under it (they differed by under 4 m until right below it)
                if (f.z - p3.z).abs() > BODY_LEVEL {
                    continue;
                }
                let rel = p - f.center;
                // the footprint grown by this car's half width across its way, less a
                // little so that a car on the lane beside does not count (10 cm: at 20 cm
                // the corners of two bodies at a shallow merge ran a hand's breadth into
                // each other, the sampled centre line never quite inside the grown box)
                let gx = f.half_w + hw * across.dot(f.right).abs() - 0.1;
                let gy = f.half_len + hw * across.dot(f.fwd).abs() - 0.1;
                if rel.dot(f.right).abs() <= gx && rel.dot(f.fwd).abs() <= gy {
                    let along = (f.fwd.dot(dir) as f32 * f.speed).max(0.0);
                    let acc = if along > 0.1 {
                        self.cars[f.car].state.acc
                    } else {
                        0.0
                    };
                    return Some((
                        Lead {
                            gap: (d - st.front).max(0.0),
                            speed: along,
                            acc,
                        },
                        f.car,
                    ));
                }
            }
            p3 = q3;
            d += step;
        }
        None
    }

    /// Where the player's vehicle is in car `i`'s way: the gap to it and how fast it moves
    /// along that way. The bus's box is stretched along its motion for the next second and
    /// a half, so a bus pulling out of a stop, turning across or reversing is seen before
    /// it is in the lane - the lanes alone saw it only once it stood in them.
    fn player_in_way(&self, i: usize, player: &PlayerBox) -> Option<Lead> {
        let car = &self.cars[i];
        if (car.vehicle.position - player.0).length() > LOOK_AHEAD_MAX as f64 + 30.0 {
            return None;
        }
        self.player_on_way(&car.state, car.half_width, player, 0.0)
    }


    /// The player's bus standing (or just starting) at a stop with its indicator out
    /// towards the traffic: a car coming up behind it in the lane beside lets it out - it
    /// stops before the room the bus pulls out into (the bus's box grown by 2.5 m towards
    /// the road), as OMSI's traffic lets a bus leave its stop. Only a car going the bus's
    /// way that can still stop comfortably: one already beside the bus, or too close to
    /// stop, drives on. The gap from the car's front, or None.
    fn letting_out(&self, i: usize, player: &PlayerBox) -> Option<f32> {
        let car = &self.cars[i];
        let st = &car.state;
        let (centre, heading, half_len, _, speed) = *player;
        // (a bus that indicates and stays for long is not waited for: it is passed)
        if self.player_signal_age > 1.0 || speed.abs() > 3.0 || (self.player_signalling > 20.0 && speed.abs() < 0.3) || (car.vehicle.position - centre).length() > 120.0 {
            return None;
        }
        let h = heading.to_radians();
        let fwd = DVec2::new(h.sin(), h.cos());
        let way_dir = (st.way_point(&self.net, 3.0) - st.way_point(&self.net, 0.0)).truncate();
        if way_dir.length() < 0.5 || way_dir.normalize().dot(fwd) < 0.8 {
            return None;
        }
        // (behind the bus: a car passing it already, or past it, is not held)
        let rel = (car.vehicle.position - centre).truncate();
        if rel.dot(fwd) > -(half_len as f64) - st.front as f64 {
            return None;
        }
        let l = self.player_on_way(st, car.half_width, player, 2.5)?;
        let comfortable = st.speed * st.speed / (2.0 * st.decel.max(1.0));
        (l.gap > 0.5 && l.gap >= comfortable).then_some(l.gap)
    }

    /// `player_in_way` for a car of half width `half_width` on the way `st` lays out (also a
    /// way it only considers taking).
    /// `widen`: the box grown by this much (m) towards the traffic (the left, or the right
    /// on a left-hand-traffic map).
    fn player_on_way(&self, st: &AiState, half_width: f32, player: &PlayerBox, widen: f64) -> Option<Lead> {
        let (centre, heading, half_len, half_w, speed) = *player;
        let h = heading.to_radians();
        let fwd = DVec2::new(h.sin(), h.cos());
        let right = DVec2::new(h.cos(), -h.sin());
        let out = if self.net.left_hand { 1.0 } else { -1.0 };
        let centre = centre + (right * (out * widen * 0.5)).extend(0.0);
        let half_w = half_w + (widen * 0.5) as f32;
        let horizon = if self.player_priority { 5.0 } else { 1.5 };
        let way_dir = (st.way_point(&self.net, 3.0) - st.way_point(&self.net, 0.0)).truncate();
        let ahead = player_reach_ahead(half_len, speed, horizon, fwd, way_dir);
        let behind = half_len as f64 + ((-speed).max(0.0) * horizon) as f64;
        let wide = (half_w + half_width + 0.35) as f64;
        let margin = PLAYER_BOX_MARGIN as f64;
        let inside = |p: DVec3| in_player_box(p, centre, fwd, right, wide, ahead + margin, behind + margin);
        let look = (st.speed * st.speed / (2.0 * st.decel) + st.speed * 2.0 + 15.0)
            .clamp(15.0, look_ahead(st.speed));
        let mut d = 0.0f32;
        let mut step = 1.5f32;
        while d <= st.front + look {
            let p = st.way_point(&self.net, d.max(0.0));
            if inside(p) {
                // where between the samples the way enters the box: the gap to a standing bus
                // used to come in steps of a metre and a half (and up to that much too long,
                // so that cars stopped closer than they meant to)
                let (mut lo, mut hi) = ((d - step).max(0.0), d);
                if d > 0.0 {
                    for _ in 0..5 {
                        let mid = 0.5 * (lo + hi);
                        if inside(st.way_point(&self.net, mid)) {
                            hi = mid;
                        } else {
                            lo = mid;
                        }
                    }
                }
                let q = st.way_point(&self.net, hi + 1.0);
                let dir = (q - st.way_point(&self.net, hi))
                    .truncate()
                    .normalize_or_zero();
                let along = (fwd.dot(dir) as f32 * speed).max(0.0);
                return Some(Lead {
                    gap: (hi - st.front).max(0.0),
                    speed: along,
                    acc: 0.0,
                });
            }
            // (coarser far off: the entry is found by halving anyway)
            step = if d < st.front + 30.0 { 1.5 } else { 3.0 };
            d += step;
        }
        None
    }

    /// Advance all cars.
    /// `player`: (centre, heading in degrees, half length, half width, speed) of the
    /// player's vehicle.
    /// The railway signals' aspects from where the trains are: a signal shows go (1, or 2
    /// with a `[speedlimit]`) while a train's route is about to enter the track its signal
    /// route covers and no other train is on it; otherwise it shows stop (and falls back to
    /// stop behind the train that passed it).
    pub fn signal_aspects(&self, routes: &[omsi_map::ailists::SignalRoute], player_rail: Option<(usize, bool)>) -> hashbrown::HashMap<i64, f32> {
        let mut out: hashbrown::HashMap<i64, f32> = hashbrown::HashMap::new();
        if routes.is_empty() {
            return out;
        }
        // per train: the map ids it stands on and those of its next lanes
        let mut trains: Vec<(i64, Vec<i64>)> = self
            .cars
            .iter()
            .filter(|c| !c.state.route.is_empty() && self.net.lanes.get(c.state.lane).map(|l| l.kind == omsi_sim::traffic::LaneKind::Rail).unwrap_or(false))
            .map(|c| {
                let here = self.net.lanes[c.state.lane].key.map(|k| k.id).unwrap_or(-1);
                let ahead = c.state.route.iter().skip(c.state.route_index + 1).take(40).filter_map(|&l| self.net.lanes.get(l).and_then(|l| l.key).map(|k| k.id)).collect();
                (here, ahead)
            })
            .collect();
        // the player's own train (driven on the rails): the lanes ahead of it the way it goes,
        // every branch at a fork (which it takes is not known yet) - its signals stayed at
        // stop, only an AI train ever cleared them
        if let Some((lane, along)) = player_rail.filter(|(l, _)| *l < self.net.lanes.len()) {
            let here = self.net.lanes[lane].key.map(|k| k.id).unwrap_or(-1);
            let mut ahead: Vec<i64> = Vec::new();
            let mut frontier = vec![lane];
            for _ in 0..6 {
                let mut next = Vec::new();
                for l in frontier {
                    let nb: Vec<usize> = if along { self.net.lanes[l].next.clone() } else { self.net.prev.get(l).cloned().unwrap_or_default() };
                    for n in nb {
                        if let Some(k) = self.net.lanes.get(n).and_then(|x| x.key) {
                            if !ahead.contains(&k.id) {
                                ahead.push(k.id);
                            }
                        }
                        next.push(n);
                    }
                }
                frontier = next;
                if frontier.len() > 32 {
                    break;
                }
            }
            trains.push((here, ahead));
        }
        for r in routes {
            let pieces: hashbrown::HashSet<i64> = r.entries.iter().map(|e| e[0]).collect();
            let occupied = trains.iter().any(|(here, _)| pieces.contains(here));
            let wanted = trains.iter().any(|(here, ahead)| !pieces.contains(here) && ahead.iter().any(|id| pieces.contains(id)));
            let aspect = if wanted && !occupied { if r.speed_limit.is_some() { 2.0 } else { 1.0 } } else { 0.0 };
            let e = out.entry(r.signal.0).or_insert(0.0);
            *e = e.max(aspect);
        }
        out
    }

    /// The switches the trains need set: (map object, `[path]` index) of the next lanes of
    /// every train's timetable route. A train throws the points ahead of it to the
    /// `[switchdir]` of the path its route takes (see `World::set_switches`).
    pub fn switch_requests(&self) -> Vec<(i64, u16)> {
        let mut out = Vec::new();
        for c in &self.cars {
            let st = &c.state;
            if st.route.is_empty() || self.net.lanes.get(st.lane).map(|l| l.kind != omsi_sim::traffic::LaneKind::Rail).unwrap_or(true) {
                continue;
            }
            for &l in st.route.iter().skip(st.route_index).take(5) {
                if let Some(k) = self.net.lanes.get(l).and_then(|l| l.key) {
                    out.push((k.id, k.path));
                }
            }
        }
        out
    }

    pub fn tick(&mut self, dt: f32, player: Option<PlayerBox>) {
        self.lamp_dt += dt;
        if self.mirror {
            self.mirror_tick(dt);
            return;
        }
        let t_start = std::time::Instant::now();
        self.time += dt;
        self.day_time += dt as f64 * self.time_scale;
        self.last_dt = dt;
        self.held_at_red = 0;
        self.player = player;
        self.geo_prev = self.cars.iter_mut().map(|c| c.geo_block.take()).collect();
        self.index_of = self
            .cars
            .iter()
            .enumerate()
            .map(|(i, c)| (c.id, i))
            .collect();
        let debug = omsi_cfg::env::var_os("OMSI_DEBUG_TRAFFIC").is_some();
        // where every car is: its lane with its lateral place (and the lane a passing car
        // is over on, where it counts for the oncoming traffic)
        let mut by_lane: HashMap<usize, Vec<(usize, f32, f32, bool)>> = HashMap::new();
        // cars coming to a junction lane: (car, distance from its origin to the lane start)
        let mut coming: HashMap<usize, Vec<(usize, f32)>> = HashMap::new();
        let mut reservations: HashMap<usize, Vec<usize>> = HashMap::new();
        for (i, c) in self.cars.iter().enumerate() {
            by_lane
                .entry(c.state.lane)
                .or_default()
                .push((i, c.state.s, c.state.lateral, false));
            // the rear of the vehicle still stands in the lane it came from: whoever crosses
            // or follows that lane waits until it is out (a bus that had turned into the next
            // lane with its front used to count as gone from the junction it still filled)
            if let Some(p) = c.state.prev_lane {
                if c.state.s < c.state.rear + 1.0 && c.state.change.is_none() {
                    by_lane.entry(p).or_default().push((
                        i,
                        self.net.lanes[p].length() + c.state.s,
                        c.state.lateral,
                        false,
                    ));
                }
            }
            if let Some(ch) = c.state.change {
                by_lane
                    .entry(ch.to)
                    .or_default()
                    .push((i, ch.s_to, 0.0, false));
            }
            if let Some(p) = c.passing {
                // Out on the oncoming lane: the traffic there stops short of the place where
                // the car will be back out of their way (it is headed there, and that place
                // does not move). One that gave up counts only while it still stands too far
                // out for them to get by.
                let deep = c.state.lateral * self.net.oncoming_sign();
                let out = if p.aborted {
                    deep > p.side - c.half_width - 1.45
                } else {
                    deep > 1.2
                };
                if out {
                    if let Some((l, s, _)) = self
                        .net
                        .opposite(c.state.lane, c.state.s)
                        .filter(|o| o.0 == p.lane || (o.2 - p.side).abs() < 1.5)
                    {
                        let r = if p.aborted {
                            0.0
                        } else {
                            (p.clear_at(c.half_width) - c.state.odometer).max(0.0)
                        };
                        let at = s - r;
                        if at >= 0.0 {
                            by_lane.entry(l).or_default().push((i, at, 0.0, true));
                        } else {
                            // (that place lies on the lanes before it)
                            for (ul, off, _) in
                                self.net.upstream(l, at, 1.0, 12).into_iter().skip(1)
                            {
                                let x = at - off;
                                if x >= 0.0 && x <= self.net.lanes[ul].length() {
                                    by_lane.entry(ul).or_default().push((i, x, 0.0, true));
                                }
                            }
                        }
                    }
                }
            }
            for (l, d) in self
                .way_lanes(&c.state, LOOK_AHEAD + 30.0)
                .into_iter()
                .skip(1)
            {
                if !self.net.crossings[l].is_empty() {
                    coming.entry(l).or_default().push((i, d));
                }
            }
            for &l in &c.reserved {
                reservations.entry(l).or_default().push(i);
            }
        }
        // the light programs: requests of whoever is coming, then the cycle clocks
        for c in self.lights.iter_mut() {
            c.request.iter_mut().for_each(|r| *r = false);
        }
        for c in &self.cars {
            for (l, d) in self.way_lanes(&c.state, 160.0) {
                if let Some((ci, li)) = self.net.lanes[l].traffic_light {
                    if let Some(ctl) = self.lights.get_mut(ci) {
                        let gap = d - c.state.front;
                        if gap <= ctl.approach_dist(li) && d > -self.net.lanes[l].length() {
                            if let Some(r) = ctl.request.get_mut(li) {
                                *r = true;
                            }
                        }
                    }
                }
            }
        }
        // the player's bus and the other players' vehicles ask too: a depot gate (Spandau's
        // `Omnibushof_S_1`, the exit arm on light 1) opens only for whoever asks, and the
        // player driving out of the depot at the start of a duty found it shut
        let askers: Vec<(DVec3, f64)> = player
            .iter()
            .map(|p| (p.0, p.1))
            .chain(self.others.iter().map(|(_, b)| (b.0, b.1)))
            .collect();
        for &(pos, heading) in &askers {
            // (off the lanes - a depot yard, a car park - a gate's lane that starts just
            // ahead, the way the bus is facing, is asked all the same: standing a few metres
            // beside every lane there, the bus never opened the barrier in front of it)
            let h = heading.to_radians();
            let fwd = glam::DVec2::new(h.sin(), h.cos());
            for l in 0..self.net.lanes.len() {
                let lane = &self.net.lanes[l];
                let Some((ci, li)) = lane.traffic_light else { continue };
                let (p0, h0) = lane.at(0.0);
                let d = (p0 - pos).truncate();
                let (along, across) = (d.dot(fwd), d.perp_dot(fwd).abs());
                let turn = ((h0 as f64 - heading + 540.0).rem_euclid(360.0) - 180.0).abs();
                if (-2.0..25.0).contains(&along) && across < 6.0 && turn < 60.0 && (p0.z - pos.z).abs() < 4.0 {
                    if let Some(r) = self.lights.get_mut(ci).and_then(|c| c.request.get_mut(li)) {
                        *r = true;
                    }
                }
            }
        }
        for (pos, heading) in askers {
            for (l, d) in self.lanes_ahead_of(pos, heading, 160.0) {
                if let Some((ci, li)) = self.net.lanes[l].traffic_light {
                    if let Some(ctl) = self.lights.get_mut(ci) {
                        if d <= ctl.approach_dist(li) {
                            if let Some(r) = ctl.request.get_mut(li) {
                                *r = true;
                            }
                        }
                    }
                }
            }
        }
        let mut walkers: HashMap<usize, Vec<f32>> = HashMap::new();
        for &(l, s) in &self.walkers {
            walkers.entry(l).or_default().push(s);
            let Some(lane) = self.net.lanes.get(l) else {
                continue;
            };
            // the push button of a pedestrian light on the way
            for (ahead, dist) in lane
                .next
                .iter()
                .map(|&n| (n, lane.length() - s))
                .chain(self.net.prev.get(l).into_iter().flatten().map(|&p| (p, s)))
                .chain(std::iter::once((l, 0.0)))
            {
                if let Some((ci, li)) = self.net.lanes.get(ahead).and_then(|x| x.traffic_light) {
                    if let Some(ctl) = self.lights.get_mut(ci) {
                        if dist <= ctl.approach_dist(li).min(10.0) {
                            if let Some(r) = ctl.request.get_mut(li) {
                                *r = true;
                            }
                        }
                    }
                }
            }
        }
        let day_time = self.day_time;
        for c in self.lights.iter_mut() {
            c.start(day_time);
            c.advance(dt);
        }
        self.log_lights();
        self.player_still = match player {
            Some(p) if p.4.abs() < 0.3 => self.player_still + dt,
            _ => 0.0,
        };
        let player_standing = self.player_still;
        let others = std::mem::take(&mut self.others);
        let mut others_still: HashMap<u32, f32> = HashMap::new();
        for (id, b) in &others {
            let before = self.others_still.get(id).copied().unwrap_or(0.0);
            others_still.insert(*id, if b.4.abs() < 0.3 { before + dt } else { 0.0 });
        }
        self.others_still = others_still;
        // the indicator towards the traffic (left, or right on a left-hand-traffic map),
        // remembered across the dark half of the lamps' cycle
        let out_side = if self.net.left_hand { 2 } else { 1 };
        self.player_signal_age = if self.player_blinker == out_side { 0.0 } else { self.player_signal_age + dt };
        self.player_signalling = if self.player_signal_age < 1.0 { self.player_signalling + dt } else { 0.0 };
        // the player's vehicle and the LAN players' on the lanes, for the right of way (the
        // rear sections and the vehicles placed by hand stand, they do not come)
        let mut users: Vec<WayUser> = Vec::new();
        if let Some(p) = player.as_ref() {
            users.extend(way_user_on(&self.net, p, self.player_blinker, player_standing, self.player_priority));
        }
        for (id, b) in others.iter().filter(|(id, _)| *id < 0xFFF0_0000) {
            users.extend(way_user_on(&self.net, b, 0, self.others_still.get(id).copied().unwrap_or(0.0), false));
        }
        self.way_users = users;
        let t_plan = std::time::Instant::now();
        let mut remove = Vec::new();
        let mut frames: Vec<Option<AiFrame>> = vec![None; self.cars.len()];
        let feet = self.footprints();
        self.break_lead_pairs();
        for i in 0..self.cars.len() {
            self.plan_lane_change(i, &by_lane);
            let ahead = self.obstacle_ahead(i, look_ahead(self.cars[i].state.speed), &by_lane);
            // remember whom it lets in at a merge (a car on another lane)
            let merging = ahead
                .filter(|(_, j)| {
                    self.cars[*j].state.lane != self.cars[i].state.lane
                        && !self.cars[i]
                            .state
                            .upcoming()
                            .any(|u| u == self.cars[*j].state.lane)
                })
                .map(|(_, j)| self.cars[j].id);
            self.cars[i].merge_after = merging;
            let mut lead = ahead.map(|(l, j)| (l, Some(j)));
            // the player's bus, wherever it overlaps this car's way, or a LAN player's (the
            // nearest in the way stands for "the player's bus" in what follows)
            let (mut player, mut player_standing) = (player, player_standing);
            if let Some(p) = player.as_ref() {
                if let Some(l) = self.player_in_way(i, p) {
                    if lead.map(|x| l.gap < x.0.gap).unwrap_or(true) {
                        lead = Some((l, Some(usize::MAX)));
                    }
                }
            }
            // (or coming to the joint where this car's lane runs into its own)
            if let Some(l) = merging_lead(&self.net, &self.cars[i].state, &self.way_users) {
                if lead.map(|x| l.gap < x.0.gap).unwrap_or(true) {
                    lead = Some((l, Some(usize::MAX)));
                }
            }
            for (id, o) in &others {
                if let Some(l) = self.player_in_way(i, o) {
                    if lead.map(|x| l.gap < x.0.gap).unwrap_or(true) {
                        lead = Some((l, Some(usize::MAX)));
                        player = Some(*o);
                        player_standing = self.others_still.get(id).copied().unwrap_or(0.0);
                    }
                }
            }
            // other vehicles' bodies in the way off the lanes
            if let Some((l, j)) = self.body_in_way(i, &feet, &by_lane) {
                self.cars[i].geo_block = Some(self.cars[j].id);
                if lead.map(|x| l.gap < x.0.gap - 0.5).unwrap_or(true) {
                    if debug
                        && l.gap < 3.0
                        && l.speed < 0.5
                        && self.cars[i].stopped == 0.0
                        && self.cars[i].state.speed > 0.5
                    {
                        log::info!("t={:.1}: car {} stops for the body of car {} in its way ({:.1} m) off the lanes", self.time, self.cars[i].id, self.cars[j].id, l.gap);
                    }
                    lead = Some((l, Some(j)));
                }
            }
            if let Some((_, Some(j))) = lead {
                if j < self.cars.len() && self.cars[i].ignore_lead.is_some_and(|(id, until)| id == self.cars[j].id && (self.time as f64) < until) {
                    lead = None;
                }
            }
            // parked cars: stop behind one in the middle of the lane, swerve round one at
            // the kerb (a parked car eats the right half of the lane; the passing car
            // moves left by what is missing, and back once it is past)
            let mut parked_ahead = false;
            let mut parked_box: Option<Obb> = None;
            let kerb_swerve: Option<f32>;
            let mut squeeze: Option<u64> = None;
            {
                let car = &self.cars[i];
                let st = &car.state;
                let near_way = self.way_lanes(st, 100.0);
                let passing = car.passing.map(|p| !p.aborted).unwrap_or(false);
                let mut swerve: Option<f32> = None;
                let mut stand: Option<(f32, usize, f32, f32)> = None;
                let mut check = |along: f32, lat: f32, lane: usize, at: f32| {
                    if !(-6.0..=100.0).contains(&along) {
                        return;
                    }
                    let a = lat.abs();
                    // in the way at the side the car is on now (a car pulled out onto the
                    // other half passes it)
                    let blocks = if passing {
                        (lat - st.lateral_ahead(along)).abs() < car.half_width + 0.9 + 0.2
                    } else {
                        a < 0.9
                    };
                    if blocks {
                        if along > 0.0 {
                            let gap = along - 2.3 - st.front;
                            if stand.map(|o| gap < o.0).unwrap_or(true) {
                                stand = Some((gap, lane, at, lat));
                            }
                        }
                    } else if !passing && a < car.half_width + 0.9 + 0.15 && along < 30.0 {
                        // (only as far as the two bodies would touch: OMSI's cars keep to
                        // their paths, and moved out by a margin of our own round every car
                        // at the kerb - 2.7 m from the lane's middle - the traffic of a
                        // narrow British street lined with parked cars wove to and fro
                        // across the road instead of keeping to its lane)
                        let need = (car.half_width + 0.9 + 0.15 - a) * -lat.signum();
                        swerve = Some(
                            swerve
                                .map(|w| if w.abs() > need.abs() { w } else { need })
                                .unwrap_or(need),
                        );
                    }
                };
                // (once committed to a lane change - well over, or pulling out round what
                // stands in the way - the parked cars of the lane it leaves hold it no more,
                // as the cars standing there do not, `obstacle_ahead`: counted still, the car
                // that had begun to pull out round a row of them stopped with its nose on the
                // first, and a lane change that moves on with the car never got anywhere -
                // six cars queued for good behind the parked row on the Heerstraße)
                let leaving = st
                    .change
                    .filter(|c| c.t > 0.4 || (c.bypass && c.wait <= 0.0))
                    .map(|_| st.lane);
                for &(l, d) in &near_way {
                    if Some(l) == leaving {
                        continue;
                    }
                    for &(s, lat) in self.parked.get(&l).map(|v| v.as_slice()).unwrap_or(&[]) {
                        check(d + s, lat, l, s);
                    }
                }
                // a bus standing half in its bay: squeeze past on
                // the other side when a metre is enough, instead of queueing behind it -
                // and stay out until past its front (moving back in while still beside it
                // steered the car into the bus)
                if !passing {
                    let swerving = st.lateral_target.abs() > 0.1;
                    for &(l, d) in &near_way {
                        for &(j, os, lat, foreign) in
                            by_lane.get(&l).map(|v| v.as_slice()).unwrap_or(&[])
                        {
                            let o = &self.cars[j];
                            let along = d + os;
                            if foreign
                                || j == i
                                || lat.abs() < 0.5
                                || !(-(o.state.front + st.rear + 1.0)..=40.0).contains(&along)
                            {
                                continue;
                            }
                            // a bus that is about to pull away (its last seconds at the stop, the
                            // indicator on) is not started round; one the car is already going
                            // round is passed, unless the car can still stop behind it gently
                            // (only a bus at its stop stands out of the lane on purpose: a car
                            // off the middle is squeezing past something itself)
                            let standing = o.state.speed < 0.3 && o.standing_for(self.day_time) > 3.0;
                            let keep = swerving
                                && car.squeeze == Some(o.id)
                                && (o.state.speed < 2.0
                                    || along
                                        < o.state.front
                                            + st.front
                                            + st.speed * st.speed / (2.0 * st.decel.max(1.0)));
                            if !standing && !keep {
                                continue;
                            }
                            let need = car.half_width + o.half_width + 0.35 - lat.abs();
                            if need > 0.0 && need <= 1.1 {
                                let w = need * -lat.signum();
                                if swerve.map(|v: f32| w.abs() > v.abs()).unwrap_or(true) {
                                    swerve = Some(w);
                                    squeeze = Some(o.id);
                                }
                            }
                        }
                    }
                }
                if let Some((gap, pl, ps, lat)) = stand {
                    // stop a little further back than behind a car that will move on
                    let l = Lead {
                        gap: (gap - 2.0).max(0.0),
                        speed: 0.0,
                        acc: 0.0,
                    };
                    if lead.map(|x| l.gap < x.0.gap).unwrap_or(true) {
                        lead = Some((l, None));
                        parked_ahead = true;
                        // (a parked car's box, for pulling out round it)
                        let (q, h) = self.net.lanes[pl].at(ps);
                        let hr = (h as f64).to_radians();
                        let centre = q.truncate() + DVec2::new(hr.cos(), -hr.sin()) * lat as f64;
                        parked_box = Some(Obb::vehicle(centre, h as f64, 2.3, 2.3, 0.9));
                    }
                }
                kerb_swerve = swerve;
            }
            if squeeze.is_some()
                && self.cars[i].squeeze.is_none()
                && self.first_passer.is_none()
                && !self.cars[i].is_bus()
            {
                self.first_passer = Some((self.cars[i].id, self.time));
            }
            self.cars[i].squeeze = squeeze;
            let standing = self.standing_obstacle(i, lead, parked_ahead, player_standing);
            // (a queue at a stop is passed as a whole)
            let (obstacle_len, at_stop) = match lead.and_then(|l| l.1) {
                Some(usize::MAX) => (
                    player.map(|p| p.2 * 2.0).unwrap_or(12.0),
                    player_standing > 10.0,
                ),
                Some(j) if j < self.cars.len() => self.standing_queue(j),
                _ => (4.8, false),
            };
            self.cars[i].lead_info = lead.and_then(|(l, who)| {
                who.filter(|&j| j < self.cars.len())
                    .map(|j| (self.cars[j].id, l.gap))
            });
            // Something that may stand for a while (a bus at its stop, the player's bus that
            // has stopped) is waited behind with room to pull out round it later: a car that
            // had stopped a metre behind the player's bus scraped its corner when it went
            // round, and no car can steer out of that.
            let may_stand = lead
                .map(|(l, who)| {
                    l.speed.abs() < 0.3
                        && match who {
                            Some(usize::MAX) => player.map(|p| p.4.abs() < 0.3).unwrap_or(false),
                            Some(j) if j < self.cars.len() => {
                                self.cars[j].at_stop()
                            }
                            _ => false,
                        }
                })
                .unwrap_or(false);
            // It stops `pass_room` short of it (the room its own steering needs to get out
            // round it), or as far back as it can without braking hard. The two metres taken
            // off the gap it keeps to such a thing were not enough: the car still crept up to
            // under three metres behind the player's bus and never got round it.
            let mut keep_back: Option<f32> = None;
            if standing || may_stand {
                if let Some((l, who)) = lead.filter(|_| !parked_ahead) {
                    let car = &self.cars[i];
                    let st = &car.state;
                    let real = l.gap
                        + if who == Some(usize::MAX) {
                            PLAYER_BOX_MARGIN
                        } else {
                            0.0
                        };
                    // (a timetable bus queueing for its own stop is not going round it)
                    let queues = car
                        .next_stop()
                        .map(|(ri, ss)| {
                            ri >= st.route_index
                                && st.route_distance(&self.net, ri, ss)
                                    < real + obstacle_len + st.front + 15.0
                        })
                        .unwrap_or(false);
                    let want = if queues {
                        st.min_gap + 2.0
                    } else {
                        car.pass_room.max(st.min_gap)
                    };
                    let comfortable = st.speed * st.speed / (2.0 * st.decel.max(1.0));
                    let stop_gap = if real - want >= comfortable {
                        want
                    } else {
                        // (a gap wanted under half a metre is the floor itself: clamp
                        // panicked with its bounds the wrong way round, #138)
                        (real - comfortable).clamp(real.min(0.5).min(want), want)
                    };
                    keep_back = Some(st.front + (real - stop_gap).max(0.0) + 0.6);
                }
            }
            // a pass it gave up: stop where it said it would
            if let Some(p) = self.cars[i].passing.filter(|p| p.aborted) {
                let st = &self.cars[i].state;
                let at = st.front + (p.hold - st.odometer).max(0.0) + 0.6;
                keep_back = Some(keep_back.map(|k| k.min(at)).unwrap_or(at));
            }
            self.plan_bypass(i, lead.map(|l| l.0.gap), standing, &by_lane);
            let way_now = self.way_lanes(&self.cars[i].state, 120.0);
            self.guard_pass(i, &by_lane);
            self.plan_pass(
                i,
                lead,
                obstacle_len,
                standing,
                parked_ahead || at_stop,
                &way_now,
                &by_lane,
                player,
                parked_box,
                &feet,
            );
            // passing: back into the lane once past (and give up if the way out closes
            // before the car has moved)
            {
                let car = &mut self.cars[i];
                if let Some(mut p) = car.passing {
                    if !p.aborted
                        && car.stopped > 8.0
                        && car.state.odometer < p.until
                        && car.state.lateral.abs() < p.side * 0.5
                    {
                        // held before it got out: back in behind the obstacle rather than
                        // wait half in the oncoming lane
                        p.until = car.state.odometer;
                        p.aborted = true;
                        p.hold = car.state.odometer;
                        car.passing = Some(p);
                    }
                    if p.aborted {
                        // standing behind what it gave up going round: free to look again
                        // (from where it stands, half out or not)
                        if car.state.speed < 0.1
                            && (car.state.odometer >= p.hold - 0.3 || car.stopped > 1.0)
                        {
                            car.passing = None;
                        }
                    } else if car.state.odometer >= p.until {
                        car.state.lateral_target = 0.0;
                        let odo = car.state.odometer;
                        let from = car.state.lateral;
                        if (car.state.lateral_ramp.1 - 0.0).abs() > 1e-3 {
                            // back in gently, as a driver does once past: over the S-curve
                            // planned for the speed it has here and the room ahead
                            car.state.lateral_ramp = (from, 0.0, odo, p.back);
                        }
                        if car.state.lateral.abs() < 0.05 {
                            car.passing = None;
                        }
                    }
                }
            }
            let merge_wait = self.plan_route_change(i, &by_lane);
            let way = self.way_lanes(&self.cars[i].state, 200.0);
            // traffic lights
            let light = if self.net.lanes[self.cars[i].state.lane].kind == LaneKind::Air {
                None
            } else {
                self.light_stop(i, &way)
            };
            self.cars[i].light_hold = light.is_some();
            self.cars[i].light_at = light;
            if light.is_some() && self.cars[i].state.speed < 0.5 {
                self.held_at_red += 1;
                if self.first_red.is_none()
                    && self.cars[i].state.speed < 0.2
                    && !self.cars[i].is_bus()
                {
                    self.first_red = Some((self.cars[i].id, self.time));
                }
            }
            // right of way: at every junction before the red light's line (and in the one the
            // car is in already) - skipping them all whenever some light ahead was red let a
            // car cross another's path unchecked on its way to a light further on
            let junction = if self.net.lanes[self.cars[i].state.lane].kind == LaneKind::Air {
                None
            } else {
                self.junction_ahead(&way).filter(|jn| {
                    light
                        .map(|l| jn.inside || jn.lanes[0].1 < l - 0.5)
                        .unwrap_or(true)
                })
            };
            let yield_at = match &junction {
                Some(jn) => self.junction_stop(
                    i,
                    jn,
                    &way,
                    lead.map(|l| l.0),
                    &by_lane,
                    &coming,
                    &mut reservations,
                    &walkers,
                ),
                None => {
                    let old = std::mem::take(&mut self.cars[i].reserved);
                    for l in old {
                        if let Some(list) = reservations.get_mut(&l) {
                            list.retain(|&c| c != i);
                        }
                    }
                    None
                }
            };
            // what it has claimed and is through no longer counts
            {
                let car = &mut self.cars[i];
                let on_way: Vec<usize> = way.iter().map(|w| w.0).collect();
                car.reserved.retain(|l| on_way.contains(l));
                car.yielding = yield_at.is_some();
                car.wait_at = yield_at;
                let st = &mut car.state;
                if yield_at.is_some() && st.speed < 0.3 {
                    st.yield_time += dt;
                } else if yield_at.is_none() {
                    st.yield_time = 0.0;
                }
                // (`--follow yield`: a car that has stood for a couple of seconds giving way at
                // a junction without lights)
                if car.yielding
                    && st.yield_time >= 2.0
                    && st.yield_time - dt < 2.0
                    && self.first_yield.is_none()
                    && !car.is_bus()
                    && junction
                        .as_ref()
                        .map(|j| {
                            j.lanes.iter().all(|l| {
                                self.net.lanes[l.0].traffic_light.is_none()
                                    && self.net.prev[l.0]
                                        .iter()
                                        .all(|&p| self.net.lanes[p].traffic_light.is_none())
                            })
                        })
                        .unwrap_or(false)
                {
                    self.first_yield = Some((car.id, self.time));
                }
            }
            let for_people = if self.net.lanes[self.cars[i].state.lane].kind == LaneKind::Air {
                None
            } else {
                self.people_stop(i, &way)
            };
            if let Some((at, who)) = for_people {
                let car = &self.cars[i];
                if debug && car.state.speed > 0.5 {
                    log::info!(
                        "t={:.2}: car {} stops for somebody on foot {:.1} m ahead",
                        self.time,
                        car.id,
                        at - car.state.front
                    );
                }
                // somebody who never moves out of the way (standing in the carriageway)
                if car.stopped >= 20.0 && car.stopped - dt < 20.0 {
                    log::info!(
                        "car {} has stood 20 s for somebody on foot at ({:.1}, {:.1})",
                        car.id,
                        who.x,
                        who.y
                    );
                }
            }
            let people = for_people.map(|x| x.0);
            // a car that has just left its parking space waits a moment before pulling out
            let parked_wait = (self.cars[i].pull_out > 0.0).then(|| self.cars[i].state.front + 0.1);
            self.cars[i].pull_out = (self.cars[i].pull_out - dt).max(0.0);
            // the player's bus indicating out of its stop
            let let_out = self.player.and_then(|p| self.letting_out(i, &p)).map(|g| self.cars[i].state.front + (g - 1.0).max(0.0));
            let mut stop_at = [light, yield_at, merge_wait, keep_back, people, parked_wait, let_out]
                .into_iter()
                .flatten()
                .reduce(f32::min);
            let mut why: (&'static str, f32) = ("", f32::MAX);
            for (name, v) in [("light", light), ("yield", yield_at), ("merge", merge_wait), ("keep_back", keep_back), ("people", people), ("pull_out", parked_wait), ("let_out", let_out)] {
                if let Some(v) = v {
                    if v < why.1 {
                        why = (name, v);
                    }
                }
            }
            self.cars[i].held = stop_at.is_some() || lead.map(|l| l.0.gap < 12.0).unwrap_or(false);
            // a timetable bus: its stops (see `bus_service`); any other car keeps to the middle
            // of its lane, or swerves round a car parked at the kerb
            {
                let car = &mut self.cars[i];
                if let Some(service) = car.bus.as_mut() {
                    let wanted = self.stop_wishes.as_ref().map(|(alighting, waiting)| {
                        alighting.contains(&car.id) || service.stops.front().is_some_and(|s| waiting.contains(&s.id))
                    });
                    let ctx = crate::bus_service::Ctx {
                        wanted,
                        net: &self.net,
                        way: &way,
                        day_time: self.day_time,
                        dt,
                        id: car.id,
                        stopped: car.stopped,
                        passing: car.passing.is_some(),
                        kerb_swerve,
                        debug: debug || omsi_cfg::env::var_os("OMSI_DEBUG_PAX").is_some(),
                    };
                    if let Some(at) = service.step(&mut car.state, &mut car.vehicle, &ctx) {
                        stop_at = Some(stop_at.map(|x| x.min(at)).unwrap_or(at));
                        if at < why.1 {
                            why = ("service", at);
                        }
                    }
                } else if car.passing.is_none() && car.park.is_none() {
                    // round a car parked at the kerb, else in the middle of the lane
                    let target = kerb_swerve.unwrap_or(0.0);
                    if debug && (target - car.state.lateral_target).abs() > 0.3 {
                        log::info!(
                            "t={:.2}: car {} swerves to {target:+.2} (was {:+.2}, now at {:+.2})",
                            self.time,
                            car.id,
                            car.state.lateral_target,
                            car.state.lateral
                        );
                    }
                    car.state.lateral_target = target;
                }
            }
            // parking: stop beside the space, move over into it, and stand there
            {
                let car = &mut self.cars[i];
                if let Some(mut plan) = car.park {
                    let st = &mut car.state;
                    match way.iter().find(|w| w.0 == plan.lane) {
                        None if st.lane != plan.lane => {
                            // (its way went elsewhere after all)
                            car.park = None;
                        }
                        found => {
                            let dl = found.map(|w| w.1).unwrap_or(-st.s);
                            let d = dl + plan.s; // origin to the space's middle
                            if d < 80.0 {
                                let at = d + st.front + 0.3;
                                stop_at = Some(stop_at.map(|x| x.min(at)).unwrap_or(at));
                                if at < why.1 {
                                    why = ("park", at);
                                }
                                st.signal = 2;
                                st.signal_time = st.signal_time.max(1.0);
                            }
                            // over into the space along the last metres, the move ending
                            // where the car stops (the lateral place follows the distance
                            // driven: started too late, the car stood half out of the space)
                            if car.passing.is_none() {
                                let len = (plan.lat.abs() * 6.0).clamp(10.0, 22.0);
                                if !plan.ramped && d < len + 1.0 && d > 2.0 {
                                    plan.ramped = true;
                                    st.lateral_target = plan.lat;
                                    st.lateral_ramp = (st.lateral, plan.lat, st.odometer, (d - 0.6).max(4.0));
                                } else if plan.ramped {
                                    st.lateral_target = plan.lat;
                                } else {
                                    st.lateral_target = 0.0;
                                }
                            }
                            if d.abs() < 1.5 && st.speed < 0.2 && (st.lateral - plan.lat).abs() < 0.3 {
                                plan.done = true;
                            } else if d < -4.0 {
                                car.park = None;
                                st.lateral_target = 0.0;
                            }
                            if car.park.is_some() {
                                car.park = Some(plan);
                            }
                        }
                    }
                }
            }
            // the end of the way: a timetable bus at the end of its trip drives on as
            // ordinary traffic until it is out of sight; a dead end is a place to stop
            {
                let car = &mut self.cars[i];
                let st = &mut car.state;
                let (last, end) = way
                    .last()
                    .map(|&(l, d)| (l, d + self.net.lanes[l].length()))
                    .unwrap_or((st.lane, 0.0));
                let exhausted = if st.route.is_empty() {
                    self.net.lanes[last].next.is_empty()
                } else {
                    st.route.last() == Some(&last)
                };
                let air = self.net.lanes[st.lane].kind == LaneKind::Air;
                if exhausted && st.change.is_none() && end < 150.0 {
                    let service = car.bus.as_deref_mut();
                    let in_service = service
                        .as_ref()
                        .map(|b| b.route_open || (b.stops.is_empty() && !b.at_stop()))
                        .unwrap_or(false);
                    let stops_left = service.as_ref().map(|b| !b.stops.is_empty() || b.at_stop()).unwrap_or(false);
                    let waited_out = car.stopped > ROUTE_WAIT_MAX
                        && service.as_ref().map(|b| b.route_open).unwrap_or(false);
                    if waited_out && !air {
                        // it has waited long for more route where the tiles are loaded: the
                        // rest of its trip does not join what it has (a track the map does
                        // not have any more). It stood in the carriageway for good, the
                        // traffic queued behind it; now it drives on as ordinary traffic
                        // and leaves once out of sight.
                        if let Some(b) = service {
                            b.route_open = false;
                            b.stops.clear();
                        }
                        st.route.clear();
                        st.route_index = 0;
                        st.planned_next = None;
                        st.ahead.clear();
                        st.plan_next(&self.net);
                        car.gone = true;
                        log::info!("timetable bus {} waited {:.0} s for the rest of its route at ({:.0}, {:.0}): it does not join; the bus drives on and leaves", car.id, car.stopped, car.vehicle.position.x, car.vehicle.position.y);
                    } else if !st.route.is_empty() && in_service && !air && !car.gone {
                        // the end of the route it has: where the loaded tiles end, it waits
                        // for more route (or to be taken off out of sight); at the end of its
                        // trip, it stops there and waits for the timetable
                        let at = end - 0.5;
                        stop_at = Some(stop_at.map(|x| x.min(at)).unwrap_or(at));
                        if at < why.1 {
                            why = ("end", at);
                        }
                        let b = service.unwrap();
                        if !b.route_open && st.speed < 0.3 && !b.trip_done() {
                            b.phase = Phase::TripDone;
                            b.phase_t = 0.0;
                            if debug {
                                log::info!("t={:.1}: timetable bus {} at the end of its trip", self.time, car.id);
                            }
                        }
                    } else if !st.route.is_empty() && !stops_left {
                        st.route.clear();
                        st.route_index = 0;
                        st.planned_next = None;
                        st.ahead.clear();
                        st.plan_next(&self.net);
                        car.gone = true;
                        if debug {
                            log::info!(
                                "t={:.1}: car {} finished its trip, drives on until out of sight",
                                self.time,
                                car.id
                            );
                        }
                    } else if st.route.is_empty() {
                        // a dead end (the map's edge, the end of a street spline): Omsi.exe
                        // drives on at speed and deletes the car the frame it runs out of
                        // road (0x71dc9c finds no next segment, 0x6fe3fc deletes it), and
                        // `drive` takes it off there. Braking for the end, the cars stopped
                        // there one by one and those behind queued into a stop-and-go (an
                        // aircraft flies on in any case)
                        car.gone = true;
                    }
                }
            }
            let lead_id = lead
                .and_then(|l| l.1)
                .filter(|&j| j < self.cars.len())
                .map(|j| self.cars[j].id);
            let car = &mut self.cars[i];
            car.lead_car = lead_id;
            if car.state.speed.abs() < 0.1 && !car.at_stop() {
                car.stopped += dt;
            } else {
                car.stopped = 0.0;
            }
            if car.state.speed.abs() < 1.0 && !car.at_stop() {
                car.crawl += dt;
            } else {
                car.crawl = 0.0;
            }
            if (car.state.odometer - car.progress.0).abs() > 2.0 || car.at_stop() {
                car.progress = (car.state.odometer, 0.0);
            } else {
                car.progress.1 += dt;
            }
            let stood = car.stopped.max(car.progress.1);
            // a random car that has stood for a minute without a light or a junction
            // holding it has given up: it leaves as soon as nobody sees it
            // (one yielding for minutes is in a gridlock nobody else will end)
            if (stood > 60.0 && !car.yielding || stood > 150.0) && !car.is_bus() && !car.light_hold && !car.gone
            {
                car.gone = true;
                if debug {
                    log::info!(
                        "t={:.1}: car {} stood for {:.0} s: taken off once out of sight",
                        self.time,
                        car.id,
                        stood
                    );
                }
            }
            let lane_before = car.state.lane;
            let lead_now = lead.map(|l| l.0);
            car.why = match (why.1 < f32::MAX, lead_now) {
                (_, Some(l)) if l.gap + car.state.front < why.1 => (
                    match lead.and_then(|l| l.1) {
                        Some(usize::MAX) => "player",
                        Some(_) => "lead",
                        None => "parked",
                    },
                    l.gap,
                ),
                (true, _) => (why.0, why.1 - car.state.front),
                _ => ("", 0.0),
            };
            if car.fresh > 0.0 {
                car.fresh -= dt;
                // placed moving before a queue or a red light: arrive slower rather than
                // start with an emergency stop
                for _ in 0..16 {
                    if car.state.speed < 0.5
                        || car.state.desired_accel(&self.net, lead_now, stop_at) >= -car.state.decel
                    {
                        break;
                    }
                    car.state.speed *= 0.8;
                }
                if car.state.speed < 0.5 {
                    car.state.speed = 0.0;
                }
            }
            // edging out round something standing close ahead
            car.state.accel_cap = car
                .passing
                .filter(|p| p.creep && !p.aborted && car.state.odometer < p.block + CREEP_PAST)
                .map(|_| PULL_OUT_ACCEL);
            let (speed_before, lane_now, s_now) = (car.state.speed, car.state.lane, car.state.s);
            if debug && car.stopped > 30.0 {
                car.holding = Some(format!("lead {:?} (car {:?}), stop {:?} (light {:?}, junction {:?}, merge {:?}), bus {:?}, lane {} s {:.1} of {:.1}, next {:?}, lateral {:.2}, stops {:?}", lead_now, lead_id, stop_at.map(|x| x - car.state.front), light, yield_at, merge_wait, car.bus.as_ref().map(|b| (b.phase, b.phase_t as i32)), car.state.lane, car.state.s, self.net.lanes[car.state.lane].length(), car.state.planned_next, car.state.lateral, car.next_stop()));
                if car.stopped - dt <= 30.0 {
                    log::info!(
                        "t={:.1}: car {} ({}) has stood for 30 s at ({:.0}, {:.0}): {}",
                        self.time,
                        car.id,
                        car.vehicle.ty.def.type_name,
                        car.vehicle.position.x,
                        car.vehicle.position.y,
                        car.holding.as_deref().unwrap_or("-")
                    );
                }
            }
            if omsi_cfg::env::var("OMSI_DEBUG_CAR").ok().and_then(|v| v.parse::<u64>().ok()) == Some(car.id) {
                let up: Vec<usize> = car.state.upcoming().take(4).collect();
                log::info!("t={:.2} car {}: v {:.2} lane {} s {:.1}/{:.1} upcoming {:?} bend {:.2} desired {:.2} lead {:?} stop {:?} why {:?}", self.time, car.id, car.state.speed, car.state.lane, car.state.s, self.net.lanes[car.state.lane].length(), up, car.state.curve_speed(&self.net), car.state.desired_accel(&self.net, lead_now, stop_at), lead_now.map(|l| l.gap), stop_at.map(|x| x - car.state.front), car.why);
            }
            if !car.state.drive(&self.net, dt, lead_now, stop_at) {
                if debug {
                    log::info!("t={:.1}: car {} ran out of road at {:.1} m/s: taken off", self.time, car.id, car.state.speed);
                }
                remove.push(i);
                continue;
            }
            if debug && car.state.acc < -4.5 {
                // hard braking is for emergencies: say what asked for it
                let who = match lead.and_then(|l| l.1) {
                    Some(usize::MAX) => "the player".to_string(),
                    Some(_) => format!("car {}", lead_id.unwrap_or(0)),
                    None if lead.is_some() => "a parked car".to_string(),
                    None => "-".to_string(),
                };
                log::info!("t={:.2}: car {} brakes {:.1} m/s² at {:.1} m/s on lane {lane_now} s {s_now:.2} (len {:.1}): lead {:?} ({who}), stop {:?} (light {:?}, junction {:?}, merge {:?})", self.time, car.id, car.state.acc, speed_before, self.net.lanes[lane_now].length(), lead_now, stop_at.map(|x| x - car.state.front), light.map(|x| x - car.state.front), yield_at.map(|x| x - car.state.front), merge_wait);
            }
            if car.state.lane != lane_before
                && self.first_turner.is_none()
                && !car.is_bus()
                && self.net.lanes[car.state.lane].turn != 0
            {
                self.first_turner = Some((car.id, self.time));
            }
            car.state.update_blinker(&self.net);
            if matches!(car.bus.as_ref().map(|b| b.phase), Some(Phase::Boarding | Phase::Waiting)) {
                // waiting at a stop: dark until it is about to pull away
                car.state.blinker = 0;
            }
            if omsi_cfg::env::var_os("OMSI_DEBUG_DOORS").is_some() && car.is_bus() && (self.time * 2.0).floor() != ((self.time - dt) * 2.0).floor() {
                let v = &car.vehicle;
                let g = |n: &str| v.var(n).map(|x| format!("{x:.2}")).unwrap_or("-".into());
                let st = g("AI_Scheduled_AtStation");
                if st != "0.00" || car.bus.as_ref().is_some_and(|b| b.at_stop()) {
                    log::info!("doors t={:.1} car {} {} phase {:?} speed {:.1}: AtStation {st} door {} {} {} {} target {} {} {} halte {} timer {}", self.time, car.id, v.ty.def.type_name, car.bus.as_ref().map(|b| b.phase), car.state.speed, g("door_0"), g("door_1"), g("door_2"), g("door_3"), g("doorTarget_0"), g("doorTarget_1"), g("doorTarget_2"), g("bremse_halte_sw"), g("door_AI_timer"));
                }
            }
            // An emergency vehicle (its script sets `TrafficPriority`, the stock ambulance)
            // is told `TrafficPriorityWarningNeeded` while something holds it up close ahead:
            // a car or the player's bus it catches up with or has to follow, a red light, a
            // junction it has to wait at. Its script sounds the siren on it; without the
            // variable it drove silent all day. (Behind a car at the same speed the siren
            // flickered on a strict "slower".)
            let priority_warning = car.vehicle.var("TrafficPriority").is_some_and(|v| v > 0.5)
                && (lead_now.is_some_and(|l| l.gap < PRIORITY_WARN_GAP && l.speed < car.state.speed + 0.5)
                    || stop_at.is_some_and(|x| x - car.state.front < PRIORITY_WARN_GAP));
            frames[i] = Some(AiFrame {
                speed: car.state.speed,
                odometer: car.state.odometer,
                steer_deg: 0.0,
                blinker: car.state.blinker,
                brake: car.state.braking,
                lights: self.night,
                at_station: car.at_station() as i32,
                at_station_side: car.at_station_side(),
                priority_warning,
            });
        }
        // Who can be seen: a car out of the view (and farther than the mirrors and the
        // shadows reach) leaves its animations as they are and is not drawn at all.
        if let Some(v) = self.viewer {
            for c in &mut self.cars {
                let p = c.vehicle.position;
                let r = (c.state.front + c.state.rear).abs().max(4.0) as f64 + 2.0;
                c.vehicle.ai_visuals =
                    (p - v.pos).length() < UNSEEN_NEAR || v.frames(p, r);
            }
        }
        let t_par = std::time::Instant::now();
        // The bodies and the scripts of the AI vehicles run in parallel: each car follows
        // its own way and its OMSI script is its own little machine reading only its own
        // state; with thirty cars and a dozen timetable buses they were the largest single
        // cost of a frame.
        {
            use rayon::prelude::*;
            let net = &self.net;
            type Work<'a> = (&'a AiState, &'a mut AiBody, &'a mut VehicleInstance, &'a mut AiFrame, &'a mut std::collections::VecDeque<(f64, DVec3)>);
            let mut work: Vec<Work> = self
                .cars
                .iter_mut()
                .zip(frames.iter_mut())
                .filter_map(|(c, f)| {
                    let f = f.as_mut()?;
                    Some((&c.state, &mut c.body, &mut c.vehicle, f, &mut c.rail_trail))
                })
                .collect();
            let profile = omsi_cfg::env::var_os("OMSI_PROFILE").is_some();
            // (a few cars per job: every job handed out wakes a worker, and the waking cost
            // the main thread more than a car's work)
            work.par_iter_mut()
                .with_min_len(4)
                .for_each(|(state, body, vehicle, frame, trail)| {
                    let t0 = std::time::Instant::now();
                    let ground = vehicle.ground.clone();
                    let contact = vehicle.contact.clone();
                    let rail = body.kind == MotionKind::Rail;
                    if rail {
                        record_rail_trail(trail, state.odometer as f64, state.way_point(net, 0.0));
                    }
                    let trail = &**trail;
                    let behind = |d: f64| rail_behind(trail, state, net, d);
                    body.step(
                        dt,
                        state.speed,
                        &|d| if rail && d < 0.0 { behind(-d as f64) } else { state.way_point(net, d) },
                        ground
                            .as_ref()
                            .map(|g| g.as_ref() as &dyn Fn(f64, f64) -> Option<f64>),
                        contact.as_deref(),
                    );
                    body.apply(vehicle);
                    if rail && !vehicle.trailers.is_empty() {
                        // the coupled cars (a train's, a tram's sections) on the track it
                        // came along, not dragged round the bends like a road trailer
                        vehicle.retrail(0.0, &|d| Some(behind(d)));
                    }
                    frame.steer_deg = body.steer;
                    let t1 = std::time::Instant::now();
                    vehicle.update_ai(dt, frame);
                    if profile && t0.elapsed().as_secs_f64() > 0.01 {
                        log::info!(
                            "  slow AI frame: {} body {:.1} ms, scripts {:.1} ms",
                            vehicle.ty.def.path.display(),
                            (t1 - t0).as_secs_f64() * 1000.0,
                            t1.elapsed().as_secs_f64() * 1000.0
                        );
                    }
                });
        }
        self.tick_split = [
            (t_plan - t_start).as_secs_f64(),
            (t_par - t_plan).as_secs_f64(),
            t_par.elapsed().as_secs_f64(),
        ];
        if omsi_cfg::env::var_os("OMSI_DEBUG_TRAILERS").is_some() {
            // coupled parts off the level of what pulls them (#140: trains' and articulated
            // buses' rear parts under bridges)
            for c in &self.cars {
                let mut lead_z = c.vehicle.position.z;
                for (k, t) in c.vehicle.trailers.iter().enumerate() {
                    let (pitch, axle, track) = t.debug_pose();
                    if pitch.abs() > 4.0 || (t.position.z - lead_z).abs() > 1.2 {
                        log::info!("trailer: car {} {} part {k} at ({:.1}, {:.1}, {:.2}) lead z {:.2} pitch {pitch:.1} axle {:?} track {:?} lane {} kind {:?}", c.id, c.vehicle.ty.def.type_name, t.position.x, t.position.y, t.position.z, lead_z, axle, track.map(|p| p.z), c.state.lane, self.net.lanes[c.state.lane].kind);
                    }
                    lead_z = t.position.z;
                }
            }
        }
        if debug {
            // a car pulled round harder than a driver would: what way was it given?
            for (c, fr) in self.cars.iter().zip(&frames) {
                if fr.is_none() || c.body.a_lat.abs() < 4.0 || !self.logged_hard.insert(c.id) {
                    continue;
                }
                let st = &c.state;
                let lanes: Vec<String> = std::iter::once(st.lane)
                    .chain(st.upcoming())
                    .take(5)
                    .map(|l| {
                        let l_ = &self.net.lanes[l];
                        format!(
                            "{l} ({} turn {} len {:.1} h {:.0}->{:.0} k {:.3}->{:.3})",
                            l_.name,
                            l_.turn,
                            l_.length(),
                            l_.start_heading(),
                            l_.end_heading(),
                            l_.curvature.first().copied().unwrap_or(0.0),
                            l_.curvature.last().copied().unwrap_or(0.0)
                        )
                    })
                    .collect();
                log::info!("t={:.1}: car {} {} at {:.1} m/s pulled {:.1} m/s² sideways (steering {:.1}°), bend speed {:.1}, s {:.1}, way {}", self.time, c.id, c.vehicle.ty.def.path.file_stem().unwrap_or_default().to_string_lossy(), st.speed, c.body.a_lat, c.body.steer, st.curve_speed(&self.net), st.s, lanes.join(" / "));
            }
        }
        if let Some(f) = self.trace.as_mut() {
            use std::io::Write;
            // the player's vehicle as id 0 (its box centre, half length both ways)
            if let Some((c, h, hl, hw, v)) = player {
                let _ = writeln!(f, "{:.3},0,player,{:.3},{:.3},{:.3},{:.3},0,0,0,{:.3},-1,0,0,0,0,0,0,0,0,0,0,{hl:.2},{hl:.2},{hw:.2},0,,0,None", self.time, c.x, c.y, c.z, h, v);
            }
            // (`OMSI_TRACE_AI_BUSES=1`: the timetable buses only)
            let buses_only = omsi_cfg::env::var_os("OMSI_TRACE_AI_BUSES").is_some();
            for (c, fr) in self.cars.iter().zip(&frames) {
                let Some(fr) = fr else { continue };
                if buses_only && !c.is_bus() {
                    continue;
                }
                let v = &c.vehicle;
                let lane_heading = self.net.lanes[c.state.lane]
                    .at(c.state.s)
                    .1
                    .rem_euclid(360.0);
                let _ = writeln!(f, "{:.3},{},{},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{},{:.2},{},{},{:.2},{:.2},{},{:.2},{},{},{},{:.2},{:.2},{:.2},{},{},{:.1},{:?},{:.3}", self.time, c.id, v.ty.def.path.file_stem().unwrap_or_default().to_string_lossy(), v.position.x, v.position.y, v.position.z, v.heading, v.pitch, v.bank, fr.steer_deg, c.state.speed, c.state.lane, c.state.s, fr.blinker, self.net.lanes[c.state.lane].turn, lane_heading, c.state.lateral, c.at_station() as i32, c.state.acc, c.yielding as i32, c.light_hold as i32, c.passing.is_some() as i32, c.state.front, c.state.rear, c.half_width, c.is_bus() as i32, c.why.0, c.why.1.min(999.0), c.bus.as_ref().map(|b| b.phase), self.net.lanes[c.state.lane].at(c.state.s).0.z);
            }
        }
        if omsi_cfg::env::var_os("OMSI_CHECK_OVERLAP").is_some() {
            self.check_overlaps(player, &others);
        }
        for i in remove.into_iter().rev() {
            let c = self.cars.swap_remove(i);
            self.orphan_sounds.extend(c.sounds);
            // the renders go back to the world at the next sync
            self.released.push(c.render);
            self.released.extend(c.trailer_renders);
        }
    }

    /// What holds each car that has stood for over a minute (the offscreen traffic health
    /// report): why, its blinker, its light and the lane.
    pub fn stuck_report(&self) -> Vec<String> {
        self.cars
            .iter()
            .filter(|c| c.stopped > 60.0 || (c.stopped > 15.0 && c.state.signal != 0 && c.yielding))
            .map(|c| {
                let st = &c.state;
                let light = self.way_lanes(st, 60.0).into_iter().find_map(|(l, d)| {
                    self.net.lanes[l].traffic_light.and_then(|(ci, li)| self.lights.get(ci).map(|ctl| format!("light {ci}/{li} state {} at {d:.1} (time {:.0}, held {}, cycle {:.0}, phases {:?}, stops {:?})", ctl.state(li), ctl.time, ctl.held, ctl.cycle, ctl.lights, ctl.stops)))
                });
                format!(
                    "car {} {} stood {:.0} s: why {:?} blinker {} yielding {} light_hold {} lane {} ({}) s {:.1}/{:.1} next {:?} {} lead {:?} bus {:?} pos ({:.1}, {:.1}) junction {} [geo_block {:?} squeeze {:?} wait_at {:?} held {} start_timer {:.2} accel_cap {:?} crawl {:.1} passing {} park {} pull_out {:.1} acc {:.2}]",
                    c.id,
                    c.vehicle.ty.def.path.file_stem().unwrap_or_default().to_string_lossy(),
                    c.stopped,
                    c.why,
                    st.signal,
                    c.yielding,
                    c.light_hold,
                    st.lane,
                    self.net.lanes[st.lane].name,
                    st.s,
                    self.net.lanes[st.lane].length(),
                    st.upcoming().take(3).collect::<Vec<_>>(),
                    light.unwrap_or_default(),
                    c.lead_info,
                    c.bus.as_ref().map(|b| b.phase),
                    c.vehicle.position.x,
                    c.vehicle.position.y,
                    c.junction_why,
                    c.geo_block,
                    c.squeeze,
                    c.wait_at,
                    c.held,
                    st.start_timer,
                    st.accel_cap,
                    c.crawl,
                    c.passing.is_some(),
                    c.park.is_some(),
                    c.pull_out,
                    st.acc
                )
            })
            .collect()
    }

    /// `OMSI_CHECK_OVERLAP`: every AI vehicle whose body has got into another's or into a
    /// player's bus (by more than 20 cm), once per pair and 10 s, with what each was doing.
    fn check_overlaps(&mut self, player: Option<PlayerBox>, others: &[(u32, PlayerBox)]) {
        static SEEN: std::sync::OnceLock<parking_lot::Mutex<HashMap<(u64, u64), f32>>> = std::sync::OnceLock::new();
        let seen = SEEN.get_or_init(|| parking_lot::Mutex::new(HashMap::new()));
        let feet = self.footprints();
        let boxes: Vec<(u64, Footprint)> = player
            .iter()
            .map(|b| (u64::MAX, *b))
            .chain(others.iter().map(|(id, b)| (u64::MAX - 1 - *id as u64, *b)))
            .map(|(id, (c, h, hl, hw, v))| {
                let hr = h.to_radians();
                (id, Footprint { car: usize::MAX, center: c.truncate(), fwd: DVec2::new(hr.sin(), hr.cos()), right: DVec2::new(hr.cos(), -hr.sin()), half_len: hl as f64, half_w: hw as f64, speed: v, z: c.z })
            })
            .collect();
        let why = |c: &AiCar| format!("{} v {:.1} lane {} s {:.1} lat {:.2} why {:?} passing {} change {}", c.vehicle.ty.def.path.file_stem().unwrap_or_default().to_string_lossy(), c.state.speed, c.state.lane, c.state.s, c.state.lateral, c.why, c.passing.is_some(), c.state.change.is_some());
        for (a, fa) in feet.iter().enumerate() {
            let ca = &self.cars[fa.car];
            let hit = |other: u64, fb: &Footprint, desc: String| {
                if (fa.z - fb.z).abs() > 3.0 || !fa.overlaps(fb, -0.2) {
                    return;
                }
                let key = (ca.id.min(other), ca.id.max(other));
                let mut m = seen.lock();
                if m.get(&key).is_some_and(|t| self.time - *t < 10.0) {
                    return;
                }
                m.insert(key, self.time);
                log::info!("t={:.1}: OVERLAP car {} ({}) with {} at ({:.1}, {:.1})", self.time, ca.id, why(ca), desc, fa.center.x, fa.center.y);
            };
            for fb in feet.iter().skip(a + 1) {
                if fb.car == fa.car {
                    continue;
                }
                let cb = &self.cars[fb.car];
                hit(cb.id, fb, format!("car {} ({})", cb.id, why(cb)));
            }
            for (id, fb) in &boxes {
                hit(*id, fb, format!("player box {} v {:.1}", u64::MAX - id, fb.speed));
            }
        }
    }

    /// `OMSI_DEBUG_LIGHTS`: every change of the lights of the chosen programs, with the
    /// game time and the program's cycle position.
    fn log_lights(&mut self) {
        let Some(sel) = self.light_log.as_deref() else {
            return;
        };
        let near = sel.eq_ignore_ascii_case("near");
        let chosen: Vec<usize> = sel
            .split(',')
            .filter_map(|v| v.trim().parse().ok())
            .collect();
        let viewer = self.viewer.map(|v| v.pos);
        for (ci, c) in self.lights.iter().enumerate() {
            let pick = sel.eq_ignore_ascii_case("all")
                || chosen.contains(&ci)
                || (near && viewer.is_some());
            if !pick {
                continue;
            }
            if near {
                // the programs within 150 m of the camera (by the lanes they control)
                let vp = viewer.unwrap();
                let close = self.net.lanes.iter().any(|l| {
                    l.traffic_light.map(|t| t.0) == Some(ci) && (l.start() - vp).length() < 150.0
                });
                if !close {
                    continue;
                }
            }
            for li in 0..c.lights.len() {
                let s = c.state(li);
                let prev = self.light_prev[ci][li];
                if s != prev {
                    let h = self.day_time.rem_euclid(86400.0);
                    log::info!("light {ci}.{li}: {:?} ({s}) at {:02}:{:02}:{:05.2} (cycle {:.2} of {:.0} s{}{})", TrafficLightController::aspect(s), (h / 3600.0) as u32, ((h / 60.0) % 60.0) as u32, h % 60.0, c.time, c.cycle_len(), if c.held { ", held" } else { "" }, if c.request.get(li).copied().unwrap_or(false) { ", requested" } else { "" });
                    self.light_prev[ci][li] = s;
                }
            }
        }
    }

    /// Play the `[sound_ai]` sets of the cars near `listener` (others are silenced).
    /// `street_cond` is the state of the road (see `VehicleHost::street_cond`): the stock
    /// AI sound configuration fades `WetLane_1`/`WetLane_2` in with it, which is what a car
    /// driving past through the wet sounds like. `muffled`: the listener (the player) sits in
    /// a cabin right now, so every AI car's sound is heard through that bodywork and glass -
    /// a passing car's horn does not simply sound like the street outside once the windows
    /// are shut.
    pub fn update_audio(
        &mut self,
        audio: &omsi_audio::AudioEngine,
        listener: DVec3,
        street_cond: f32,
        muffled: bool,
    ) {
        let freed = audio.trim_clips(std::time::Duration::from_secs(60));
        if freed > 0 && omsi_cfg::env::var_os("OMSI_PROFILE").is_some() {
            log::info!(
                "sound clips: {:.1} MB nobody used for a minute let go",
                freed as f64 / 1e6
            );
        }
        for mut s in self.orphan_sounds.drain(..) {
            s.stop_all(audio);
        }
        let near = 250.0;
        for c in &mut self.cars {
            let d = (c.vehicle.position - listener).length();
            if d > near * 1.2 {
                if let Some(mut s) = c.sounds.take() {
                    s.stop_all(audio);
                }
                continue;
            }
            if c.sounds.is_none() && d < near {
                let def = &c.vehicle.ty.def;
                let Some(rel) = def.sound_ai.clone().or_else(|| def.sound.clone()) else {
                    continue;
                };
                let path = omsi_cfg::resolve_path(def.dir(), &rel);
                let cfg = self
                    .sound_cfgs
                    .entry(path.clone())
                    .or_insert_with(|| {
                        omsi_vehicle::SoundCfg::load(&path)
                            .map_err(|e| log::warn!("{e}"))
                            .ok()
                            .map(Arc::new)
                    })
                    .clone();
                if let Some(cfg) = cfg {
                    let dir = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
                    // an articulated bus's rear section sounds too (its engine, on a pusher)
                    let mut parts = Vec::new();
                    for (i, t) in c.vehicle.trailers.iter().enumerate() {
                        let def = &t.ty.def;
                        let Some(rel) = def.sound_ai.clone().or_else(|| def.sound.clone()) else {
                            continue;
                        };
                        let path = omsi_cfg::resolve_path(def.dir(), &rel);
                        let part = self
                            .sound_cfgs
                            .entry(path.clone())
                            .or_insert_with(|| {
                                omsi_vehicle::SoundCfg::load(&path)
                                    .map_err(|e| log::warn!("{e}"))
                                    .ok()
                                    .map(Arc::new)
                            })
                            .clone();
                        if let Some(part) = part {
                            let dir = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
                            parts.push((i, part, dir));
                        }
                    }
                    // the clips are read in the background the first time; silent till then
                    let ready = audio.clips_ready(&omsi_audio::SoundSet::clip_paths(&cfg, &dir))
                        && parts.iter().all(|(_, part, dir)| {
                            audio.clips_ready(&omsi_audio::SoundSet::clip_paths(part, dir))
                        });
                    if ready {
                        let number = c.vehicle.number();
                        let mut ss = omsi_audio::SoundSet::new_exterior(audio, &cfg.chosen_for(&number), &dir);
                        for (i, part, dir) in &parts {
                            ss.add_part(*i, omsi_audio::SoundSet::new_exterior(audio, &part.chosen_for(&number), dir));
                        }
                        ss.master = crate::sound_gain(&crate::SOUND_AI);
                        c.vehicle.host.snapshot_triggers = ss.curve_triggers().into_iter().collect();
                        c.sounds = Some(ss);
                    }
                }
            }
            let fired: Vec<String> = std::mem::take(&mut c.vehicle.host.fired_triggers);
            let fired_vars: Vec<(String, Vec<f32>)> = std::mem::take(&mut c.vehicle.host.fired_trigger_vars);
            let fired_files: Vec<(String, String)> =
                std::mem::take(&mut c.vehicle.host.fired_file_triggers);
            c.vehicle.host.street_cond = street_cond;
            c.vehicle.set_engine_var("StreetCond", street_cond);
            if let Some(ss) = c.sounds.as_mut() {
                ss.set_muffled(muffled);
                let xf = c.vehicle.world_transform();
                let v = &c.vehicle;
                let at_fire = |t: &str, n: &str| -> Option<f32> {
                    let vals = &fired_vars.iter().rev().find(|(k, _)| k.eq_ignore_ascii_case(t))?.1;
                    v.var_slot(n).and_then(|i| vals.get(i).copied())
                };
                ss.update_fired(audio, &|n| v.var(n), &xf, &fired, &at_fire);
                ss.update_parts(
                    audio,
                    &|n| v.var(n),
                    &|i| v.trailers.get(i).map(|t| t.world_transform()),
                    &fired,
                );
                for (t, f) in &fired_files {
                    ss.play_file_trigger(audio, t, f, &|n| v.var(n), &xf);
                }
            }
        }
    }

    /// Would a vehicle of type `ty` with its origin at `pos`, heading `heading` (and its
    /// coupled parts, straight behind it) touch one that is already there - an AI vehicle,
    /// or one of `keep_clear`? Bodies are compared with half a metre to spare, not centres:
    /// an articulated bus reaches 6 m ahead of its origin and 12 m behind it, and one put
    /// down 9.3 m from the player's bus stood 1.8 m inside it.
    pub fn blocked(&mut self, ty: &Arc<VehicleType>, pos: DVec3, heading: f64) -> bool {
        let grown = |mut b: omsi_sim::collision::Obb| {
            b.half += glam::DVec2::splat(0.5);
            b
        };
        let mut bodies = vec![grown(omsi_sim::collision::Obb::from_box(
            ty.def.bounding_box.unwrap_or(DEFAULT_BOX),
            pos,
            heading,
        ))];
        let (mut origin, mut lead, mut lead_rev) = (pos, ty.clone(), false);
        for (t, rev) in self.trailer_chain(ty) {
            let (back, front) = match omsi_sim::vehicle::coupling_offsets(&lead, lead_rev, &t, rev) {
                Some((back, front)) => (back, front),
                None => {
                    // the declared joint, each end the one the part's own way names (as
                    // `TrailerPart::new_ex` takes it): a part turned round couples by its
                    // `[coupling_front]`, not by its `[coupling_back]`
                    let cb = if lead_rev { lead.def.coupling_front.as_ref() } else { lead.def.coupling_back.as_ref() };
                    let cf = if rev { t.def.coupling_back.as_ref() } else { t.def.coupling_front.as_ref() };
                    (
                        cb.map(|c| c.pos[1]).unwrap_or(if lead_rev { 4.0 } else { -4.0 }),
                        cf.map(|c| c.pos[1]).unwrap_or(if rev { -4.0 } else { 4.0 }),
                    )
                }
            };
            // each car stands along the consist's heading, turned round by its own
            // (absolute) orientation - never by the car in front of it
            let (center, car_heading) = omsi_sim::vehicle::coupling_placement(
                origin,
                heading,
                lead_rev,
                back,
                rev,
                front,
            );
            bodies.push(grown(omsi_sim::collision::Obb::from_box(
                t.def.bounding_box.unwrap_or(DEFAULT_BOX),
                center,
                car_heading,
            )));
            origin = center;
            lead = t;
            lead_rev = rev;
        }
        let reach = bodies
            .iter()
            .map(|b| (b.center - pos.truncate()).length() + b.half.length())
            .fold(0.0, f64::max)
            + 40.0;
        let touches = |o: &omsi_sim::collision::Obb| bodies.iter().any(|b| b.overlaps(o));
        self.keep_clear.iter().any(|o| touches(o))
            || self
                .cars
                .iter()
                .filter(|c| (c.vehicle.position - pos).length() < reach)
                .any(|c| vehicle_bodies(&c.vehicle).iter().any(|o| touches(o)))
    }

    /// The lanes the other way along one-way paths `lanes` (see `Schedule`'s route
    /// building: OMSI's timetable buses drive a path against its direction where the trip's
    /// station links or track say so). Only timetable routes use them: they carry no traffic
    /// and no light. Returns how many were added.
    pub fn add_reverse_twins(&mut self, lanes: &[usize]) -> usize {
        let mut new = Vec::new();
        for &l in lanes {
            if !self.twinned.insert(l) {
                continue;
            }
            let o = &self.net.lanes[l];
            let pts: Vec<DVec3> = o.points.iter().rev().copied().collect();
            let mut t = omsi_sim::traffic::LaneBuilder::polyline(pts, o.kind, o.width);
            t.key = o.key;
            t.reversed = !o.reversed;
            t.speed_limit_kmh = o.speed_limit_kmh;
            t.source = o.source;
            t.offset = o.offset;
            t.name = o.name.clone();
            t.invisible = o.invisible;
            t.priority = o.priority;
            t.density = 0.0;
            t.no_cars = true;
            new.push(t);
        }
        let n = new.len();
        if n > 0 {
            let added = self.net.extend(new, 1.5);
            log::debug!("traffic: {n} lanes added for timetable routes that drive a one-way path the other way ({:?})", added);
        }
        n
    }

    /// A lane from the end of `a` to the start of `b`, for a timetable route that jumps a
    /// gap in the map (a road piece deleted after the timetable's tracks were recorded - a
    /// dozen such holes of 10 to 150 m on Novi Sad): a smooth curve with the two lanes'
    /// headings at its ends, used by the timetable only. None when the two do not line up.
    pub fn add_connector(&mut self, a: usize, b: usize) -> Option<usize> {
        let (la, lb) = (&self.net.lanes[a], &self.net.lanes[b]);
        let (p0, p1) = (la.end(), lb.start());
        let d = (p1 - p0).truncate();
        let len = d.length();
        if !(2.0..=150.0).contains(&len) || la.kind != lb.kind {
            return None;
        }
        let dir = |h: f32| {
            let r = (h as f64).to_radians();
            glam::DVec2::new(r.sin(), r.cos())
        };
        let (t0, t1) = (dir(la.end_heading()), dir(lb.start_heading()));
        let chord = d / len;
        // the lanes point along the gap or turn across it (a junction whose turning path
        // the map lost, 66 of them on Novi Sad), never a reversal
        if t0.dot(chord) < 0.3 || t1.dot(chord) < 0.3 || t0.dot(t1) < -0.2 {
            return None;
        }
        // (tangents as long as the gap make a straight gap a smooth S; across a turn they
        // would swing out past the corner)
        let len_t = len * if t0.dot(t1) > 0.9 { 1.0 } else { 0.6 };
        let n = ((len / 3.0).ceil() as usize).max(2);
        let pts: Vec<DVec3> = (0..=n)
            .map(|i| {
                let t = i as f64 / n as f64;
                let (h00, h10, h01, h11) = (2.0 * t * t * t - 3.0 * t * t + 1.0, t * t * t - 2.0 * t * t + t, -2.0 * t * t * t + 3.0 * t * t, t * t * t - t * t);
                let xy = p0.truncate() * h00 + t0 * len_t * h10 + p1.truncate() * h01 + t1 * len_t * h11;
                DVec3::new(xy.x, xy.y, p0.z + (p1.z - p0.z) * t)
            })
            .collect();
        let mut l = omsi_sim::traffic::LaneBuilder::polyline(pts, la.kind, la.width);
        l.speed_limit_kmh = la.speed_limit_kmh.min(lb.speed_limit_kmh);
        l.name = "(timetable connector)".into();
        l.density = 0.0;
        l.no_cars = true;
        let added = self.net.extend(vec![l], 1.5);
        log::debug!("traffic: a {len:.0} m connector lane {} from lane {a} to lane {b} for a timetable route", added.start);
        Some(added.start)
    }

    /// Hand timetable bus `ci` the next trip of its tour: its route from the lane it is on
    /// (`route[0]` is that lane, `s` where it is on it) and the trip's stops. It stays where
    /// it stands; a stop right there is served in place (its layover).
    pub fn reroute(&mut self, ci: usize, route: Vec<usize>, s: f32, stops: Vec<(usize, f32, f32, f64, i64, f32)>, layover: bool) {
        let net = &self.net;
        let car = &mut self.cars[ci];
        let lane = car.state.lane;
        car.state.route = route;
        car.state.route_index = 0;
        car.state.lane = lane;
        car.state.s = s;
        car.state.change = None;
        car.state.planned_next = None;
        car.state.ahead.clear();
        car.state.plan_next(net);
        car.gone = false;
        let stops = stops.into_iter().map(crate::bus_service::Stop::from_tuple).collect();
        match car.bus.as_mut() {
            Some(b) => b.restart(stops, layover),
            None => {
                let mut b = BusService::new(stops);
                b.layover = layover;
                car.bus = Some(Box::new(b));
            }
        }
    }

    /// Let timetable bus `ci` go at the end of its trip: it drives on as other traffic and
    /// is taken off as soon as nobody sees it.
    pub fn release(&mut self, ci: usize) {
        let net = &self.net;
        let car = &mut self.cars[ci];
        let st = &mut car.state;
        st.route.clear();
        st.route_index = 0;
        st.planned_next = None;
        st.ahead.clear();
        st.plan_next(net);
        if let Some(b) = car.bus.as_mut() {
            b.restart(Vec::new(), false);
        }
        car.gone = true;
    }

    /// Take a car off the road now (the player took over its tour).
    /// The AI on the roads: (cars, buses, cars asleep far from everybody, parked cars).
    pub fn counts(&self) -> (usize, usize, usize, usize) {
        let buses = self.cars.iter().filter(|c| c.is_bus()).count();
        (self.cars.len() - buses, buses, self.dormant.len(), self.parked.values().map(Vec::len).sum())
    }

    pub fn remove_car(
        &mut self,
        world: &World,
        renderer: &Renderer,
        scene: &mut Scene,
        id: u64,
    ) -> bool {
        let Some(i) = self.cars.iter().position(|c| c.id == id) else {
            return false;
        };
        let c = self.cars.swap_remove(i);
        self.orphan_sounds.extend(c.sounds);
        for r in std::iter::once(c.render).chain(c.trailer_renders) {
            world.release_vehicle(renderer, scene, r);
        }
        true
    }

    /// Obstacle boxes of all AI vehicles (for the player's collisions), with the rear
    /// sections of articulated buses and the trailers.
    pub fn boxes(&self, near: DVec3, radius: f64) -> Vec<omsi_sim::collision::Obb> {
        self.cars
            .iter()
            .filter(|c| (c.vehicle.position - near).length() < radius)
            .flat_map(|c| {
                let bb = c
                    .vehicle
                    .ty
                    .def
                    .bounding_box
                    .unwrap_or([2.0, 4.5, 1.6, 0.0, 0.0, 0.8]);
                // moving, and with a mass of its own: a car that runs into the bus is no
                // bulldozer
                let h = c.vehicle.heading.to_radians();
                let v = glam::DVec2::new(h.sin(), h.cos()) * c.state.speed as f64;
                let (mass, id) = (c.vehicle.physics.mass_kg, c.id);
                let rear = c.vehicle.trailers.iter().filter_map(move |t| {
                    t.ty.def.bounding_box.map(|bb| {
                        omsi_sim::collision::Obb::from_box(bb, t.position, t.heading)
                            .moving(v, mass, id)
                    })
                });
                std::iter::once(
                    omsi_sim::collision::Obb::from_box(bb, c.vehicle.position, c.vehicle.heading)
                        .moving(v, mass, id),
                )
                .chain(rear)
            })
            .collect()
    }

    /// Position and heading of a car by id (None once it is gone).
    pub fn car_pose(&self, id: u64) -> Option<(DVec3, f64)> {
        self.cars
            .iter()
            .find(|c| c.id == id)
            .map(|c| (c.vehicle.position, c.vehicle.heading))
    }

    /// State of light `li` of controller `c` now and the seconds it keeps showing it (the
    /// pedestrians only start across on a green that lasts).
    pub fn light_state(&self, c: usize, li: usize) -> Option<(i32, f32)> {
        let ctl = self.lights.get(c)?;
        Some((ctl.state(li), ctl.remaining(li)))
    }

    /// Seconds until light `li` of controller `c` lets traffic go (0 while it does).
    pub fn light_until_go(&self, c: usize, li: usize) -> Option<f32> {
        self.lights.get(c)?.time_until_go(li)
    }

    /// Tell a scheduled bus's script who wants in or out (`PAX_Entry<i>_Req`,
    /// `PAX_Exit<i>_Req`): the stock AI door scripts open the rear doors only for a stop
    /// request, which comes from the exit requests.
    pub fn set_pax_requests(&mut self, id: u64, entry: &[bool], exit: &[bool]) {
        if let Some(c) = self.cars.iter_mut().find(|c| c.id == id) {
            for (i, r) in entry.iter().enumerate() {
                c.vehicle
                    .set_var(&format!("PAX_Entry{i}_Req"), *r as i32 as f32);
            }
            for (i, r) in exit.iter().enumerate() {
                c.vehicle
                    .set_var(&format!("PAX_Exit{i}_Req"), *r as i32 as f32);
            }
        }
    }

    /// Keep a scheduled bus at its stop for at least `secs` more with the doors open:
    /// passengers are still queueing at a door or stepping in.
    /// The passengers' wishes for the timetable buses' next stops (see `stop_wishes`).
    pub fn set_stop_wishes(&mut self, alighting: hashbrown::HashSet<u64>, waiting: hashbrown::HashSet<i64>) {
        self.stop_wishes = Some((alighting, waiting));
    }

    pub fn hold_boarding(&mut self, id: u64, stop: Option<i64>, secs: f32) {
        if let Some(c) = self.cars.iter_mut().find(|c| c.id == id) {
            if let Some(b) = c.bus.as_mut() {
                b.hold(stop, secs);
            }
        }
    }

    /// The street lanes a vehicle standing at `pos` facing `heading` is on and will drive
    /// onto within `reach` metres, each with the distance from the vehicle to its start
    /// (0 for the lane it is on): the way straight on and the gentle turns (within 60° of
    /// the lane before), not every branch of a junction.
    fn lanes_ahead_of(&self, pos: DVec3, heading: f64, reach: f32) -> Vec<(usize, f32)> {
        let Some((lane, s, _)) = self.net.lane_along(pos, heading, LaneKind::Street, 4.0, 45.0) else {
            return Vec::new();
        };
        let mut out = vec![(lane, 0.0f32)];
        let mut open = vec![(lane, self.net.lanes[lane].length() - s)];
        while let Some((l, to_end)) = open.pop() {
            if to_end > reach || out.len() > 64 {
                continue;
            }
            let end_heading = self.net.lanes[l].headings.last().copied().unwrap_or(0.0);
            for &n in &self.net.lanes[l].next {
                let nl = &self.net.lanes[n];
                if nl.kind != LaneKind::Street || out.iter().any(|o| o.0 == n) {
                    continue;
                }
                let start = nl.headings.first().copied().unwrap_or(end_heading);
                let turn = ((start - end_heading) as f64 + 540.0).rem_euclid(360.0) - 180.0;
                if turn.abs() > 60.0 {
                    continue;
                }
                out.push((n, to_end));
                open.push((n, to_end + nl.length()));
            }
        }
        out
    }


    /// The drivers of the timetable buses near the camera: made when a bus comes within
    /// `DRIVER_NEAR`, posed every sync, let go when it is twice that far or gone.
    fn sync_drivers(&mut self, world: &World, renderer: &Renderer, scene: &mut Scene) {
        let Some(eye) = self.viewer.map(|v| v.pos) else { return };
        let dt = self.last_dt.max(1.0 / 120.0);
        let mut keep: Vec<u64> = Vec::new();
        for c in &self.cars {
            if !c.is_bus() || c.gone {
                continue;
            }
            let d = (c.vehicle.position - eye).length();
            if d > DRIVER_NEAR * 2.0 {
                continue;
            }
            keep.push(c.id);
            if !self.drivers.contains_key(&c.id) {
                if d > DRIVER_NEAR {
                    continue;
                }
                let figure = match self.driver_pool.pop() {
                    Some(mut f) => {
                        if f.attach(&c.vehicle) {
                            Some(f)
                        } else {
                            self.driver_pool.push(f);
                            None
                        }
                    }
                    None => crate::driver::DriverFigure::new(world, renderer, scene, &c.vehicle, c.id),
                };
                match figure {
                    Some(f) => {
                        self.drivers.insert(c.id, f);
                    }
                    None => continue,
                }
            }
            if let Some(f) = self.drivers.get_mut(&c.id) {
                f.update(renderer, scene, &c.vehicle, &c.render, dt, true, false);
            }
        }
        let gone: Vec<u64> = self.drivers.keys().copied().filter(|id| !keep.contains(id)).collect();
        for id in gone {
            if let Some(mut f) = self.drivers.remove(&id) {
                f.hide(renderer, scene);
                self.driver_pool.push(f);
            }
        }
    }

    /// The `TrafficLightPhase` and `TrafficLightApproach` values of light `li` of
    /// controller `c` (for scenery scripts).
    pub fn light_vars(&self, c: usize, li: usize) -> (f32, f32) {
        self.lights
            .get(c)
            .map(|ctl| {
                (
                    ctl.state(li) as f32,
                    ctl.request.get(li).copied().unwrap_or(false) as i32 as f32,
                )
            })
            .unwrap_or((omsi_sim::traffic::UNLINKED_PHASE as f32, 0.0))
    }

    pub fn sync(&mut self, world: &World, renderer: &Renderer, scene: &mut Scene) {
        // cars that have parked: the parked object stands in their place from now on
        let mut i = 0;
        while i < self.cars.len() {
            match self.cars[i].park {
                Some(p) if p.done => {
                    let c = self.cars.swap_remove(i);
                    if world.return_parked(renderer, scene, p.key) {
                        if let Some(list) = self.parked.get_mut(&p.lane) {
                            list.push((p.s, p.lat));
                        } else {
                            self.parked.insert(p.lane, vec![(p.s, p.lat)]);
                        }
                    }
                    if omsi_cfg::env::var_os("OMSI_DEBUG_TRAFFIC").is_some() {
                        log::info!("car {} has parked (space {})", c.id, p.key);
                    }
                    self.orphan_sounds.extend(c.sounds);
                    self.released.push(c.render);
                    self.released.extend(c.trailer_renders);
                }
                _ => i += 1,
            }
        }
        for r in std::mem::take(&mut self.released) {
            world.release_vehicle(renderer, scene, r);
        }
        self.sync_drivers(world, renderer, scene);
        // traffic light lamps: the lamp's script (or the stock rules) turns the state of its
        // light into the `[visible] red|yellow|green 1` meshes and the coronas, and moves
        // what it animates (a barrier arm); it runs on the time since the last sync (an
        // offscreen run syncs only for its pictures)
        let dt = std::mem::take(&mut self.lamp_dt);
        let near = self.viewer.map(|v| v.pos);
        let debug_lamps = omsi_cfg::env::var_os("OMSI_DEBUG_LAMPS").is_some();
        for lamp in world.light_objects.lock().iter_mut() {
            if debug_lamps && lamp.animated {
                log::info!("moving lamp at ({:.1}, {:.1}), {:.0} m from the viewer", lamp.pos.x, lamp.pos.y, near.map(|p| (lamp.pos - p).length()).unwrap_or(0.0));
            }
            if let Some(p) = near {
                if (lamp.pos - p).length() > 1200.0 {
                    continue;
                }
            }
            let (state, request) = match self
                .controller_of_object
                .get(&lamp.parent)
                .and_then(|&c| self.lights.get(c))
            {
                Some(ctl) if lamp.any_light && ctl.lights.len() > 1 => {
                    // the most open of the crossing's lights (see `LightObject::any_light`)
                    let open = |s: i32| match s {
                        6..=8 => 3,
                        3..=5 => 2,
                        9..=11 => 1,
                        0..=2 => 0,
                        _ => -1,
                    };
                    let li = (0..ctl.lights.len())
                        .max_by_key(|&i| open(ctl.state(i)))
                        .unwrap_or(0);
                    (ctl.state(li), ctl.request.iter().any(|r| *r))
                }
                Some(ctl) => (
                    ctl.state(lamp.index),
                    ctl.request.get(lamp.index).copied().unwrap_or(false),
                ),
                // a lamp that names no crossing, or one whose crossing has no program,
                // reads the engine's dummy (see `UNLINKED_PHASE`): red, as in OMSI
                None => (omsi_sim::traffic::UNLINKED_PHASE, false),
            };
            let (r, y, g) = TrafficLightController::lamps(state);
            let value = |lamp: &crate::scene::LightObject, var: &str| -> f32 {
                // Custom signals can shift phases or blink the standard channels (Numazu
                // pedestrian lamps). Use their script outputs whenever they are available.
                let scripted = lamp.script.as_ref().and_then(|script| {
                    let s = script.lock();
                    // A failed/missing script can still have a varlist of zeroes. Keep
                    // stock fallback behaviour if it has no runnable frame block.
                    if s.program.frame.is_empty() { None } else { s.var(var) }
                });
                crate::scene::traffic_lamp_value(
                    var,
                    scripted,
                    crate::scene::standard_traffic_lamp(var, r, y, g, request),
                )
            };
            if let Some(script) = lamp.script.as_ref() {
                let vars = omsi_sim::scenery::SceneryVars {
                    nightlight: self.night as i32 as f32,
                    in_use: 1.0,
                    traffic_light_phase: state as f32,
                    traffic_light_approach: request as i32 as f32,
                    switch: None,
                };
                let mut s = script.lock();
                s.update(dt, &vars);
                // `OMSI_DEBUG_LAMPS`: where the moving lamps are (barriers) and how far their
                // meshes are turned, each time the lamps are updated
                if lamp.animated && debug_lamps {
                    let turn = s
                        .mesh_transforms
                        .iter()
                        .map(|m| {
                            let (_, r, _) = m.to_scale_rotation_translation();
                            r.to_axis_angle().1.to_degrees()
                        })
                        .fold(0.0f32, f32::max);
                    log::info!("lamp at ({:.1}, {:.1}): light state {:?} (crossing {:?}, light {}{}), meshes turned up to {turn:.0} deg", lamp.pos.x, lamp.pos.y, vars.traffic_light_phase, self.controller_of_object.get(&lamp.parent), lamp.index, if lamp.any_light { ", any" } else { "" });
                }
                if lamp.animated {
                    for (i, (inst, _)) in lamp.instances.iter().enumerate() {
                        if let Some(m) = s.mesh_transforms.get(i) {
                            renderer.set_transform(scene, *inst, lamp.pos, lamp.xf * *m);
                        }
                    }
                    // the lights go with their meshes (a barrier's lamps rise with its arm)
                    for (c, (mi, local, dir)) in lamp.coronas.iter_mut().zip(&lamp.corona_mesh) {
                        if let Some(m) = s.mesh_transforms.get(*mi) {
                            let xf = lamp.xf * *m;
                            c.0.position = lamp.pos + xf.transform_point3(*local).as_dvec3();
                            if *dir != glam::Vec3::ZERO {
                                c.0.direction = xf.transform_vector3(*dir).normalize_or_zero();
                            }
                        }
                    }
                }
            }
            // Traffic lamps do not enter World's ordinary scripted-object update path.
            // Switch their materials here too, so [matl_item] nightmaps light the LEDs.
            for (inst, slot, base, item, var) in &lamp.variants {
                renderer.set_material(
                    scene,
                    *inst,
                    *slot,
                    if crate::scene::change_picks_item(value(lamp, var)) { *item } else { *base },
                );
            }
            for k in 0..lamp.coronas.len() {
                let v = value(lamp, &lamp.coronas[k].1);
                lamp.lit[k] = v;
            }
            for (k, (inst, cond)) in lamp.instances.iter().enumerate() {
                let visible = match cond {
                    Some((var, want)) => (value(lamp, var) - want).abs() < 0.5,
                    None => true,
                };
                // lenses switched by their material instead (`[alphascale]` and
                // `[matl_lightmap]` on the lamp's variables, #826)
                match lamp.slots.get(k).filter(|s| !s.is_empty()) {
                    Some(slots) => {
                        let known = |v: &str| -> Option<f32> {
                            let scripted = lamp.script.as_ref().and_then(|script| {
                                let s = script.lock();
                                if s.program.frame.is_empty() { None } else { s.var(v) }
                            });
                            scripted
                                .or_else(|| v.trim().parse::<f32>().ok())
                                .or_else(|| crate::scene::standard_traffic_lamp(v, r, y, g, request))
                        };
                        let (alpha, light) = slots.values(&known);
                        renderer.set_params(scene, *inst, &alpha, visible, &[]);
                        renderer.set_slot_light(scene, *inst, &light);
                    }
                    None => renderer.set_params(scene, *inst, &[], visible, &[]),
                }
            }
        }
        // A far car's script textures (its destination sign) stay as they are drawn: OMSI
        // shows them at any distance its model level has them. (They were stood in for by
        // their mean colour beyond 50 m, and every timetable bus coming up the street had
        // a blank sign until it was almost there.) What a far car's scripts redraw goes to
        // the GPU at most every half second, a slice of the cars per frame.
        let tick = (self.time as f64 * 2.0) as u64;
        let mut budget = SCRIPT_UPLOAD_BUDGET;
        // `Envir_Brightness`, which Omsi.exe sets for every road vehicle as for the
        // player's: the stock buses fade their windows by it at night (left at the engine's
        // default of 1, an AI bus under the street lamps kept its daytime brown glass)
        if let Some(d) = self.daylight {
            for c in self.cars.iter_mut().filter(|c| c.vehicle.ai_visuals) {
                let b = d.envir_brightness(world.light_map_light_at(c.vehicle.position));
                c.vehicle.set_var("Envir_Brightness", b);
            }
        }
        for c in &mut self.cars {
            // out of sight (`tick` decided): hidden once, then left alone until it comes
            // into view again - its many per-mesh updates were a third of this stage
            if !c.vehicle.ai_visuals {
                if !c.render.hidden {
                    c.render.hidden = true;
                    for inst in c
                        .render
                        .instances
                        .iter()
                        .chain(c.trailer_renders.iter().flat_map(|r| r.instances.iter()))
                    {
                        renderer.set_params(scene, *inst, &[], false, &[]);
                    }
                }
                continue;
            }
            c.render.hidden = false;
            if let Some(cam) = self.camera {
                let far = (c.vehicle.position - cam).length() > crate::scene::DISPLAYS_FAR;
                let due = c.render.display_tick != tick;
                c.render.displays_far = far && !due;
                if far && due {
                    c.render.display_tick = tick;
                }
            }
            crate::scene::sync_vehicle_textures(renderer, scene, &mut c.vehicle, &c.render, &mut budget);
            crate::scene::sync_vehicle_materials(renderer, scene, &c.vehicle, &mut c.render);
            // a coupled part runs no scripts of its own: its plates, its displays and its
            // switched materials follow the leading vehicle's, as the player's own rear
            // sections do (without this an AI bus's rear section kept the blank textures and
            // the unswitched materials it was built with)
            {
                let mut trailers = std::mem::take(&mut c.vehicle.trailers);
                for (t, r) in trailers.iter_mut().zip(c.trailer_renders.iter_mut()) {
                    crate::scene::sync_vehicle_part(renderer, scene, &c.vehicle, t, r);
                }
                c.vehicle.trailers = trailers;
            }
            // an articulated AI bus (timetable or random traffic) bends its bellows like the
            // player's while it is near enough for the fold to show; farther out its shape
            // just stays as it was, which nobody can tell from still following the road
            if !c.render.skinned.is_empty()
                || c.trailer_renders.iter().any(|r| !r.skinned.is_empty())
            {
                let near = self
                    .camera
                    .map(|cam| (c.vehicle.position - cam).length() < SKIN_DISTANCE)
                    .unwrap_or(true);
                if near {
                    crate::scene::sync_skinned(
                        renderer,
                        scene,
                        &mut c.vehicle,
                        &mut c.render,
                        &mut c.trailer_renders,
                    );
                }
            }
            for (i, inst) in c.render.instances.iter().enumerate() {
                renderer.set_transform(
                    scene,
                    *inst,
                    c.vehicle.position,
                    c.vehicle.mesh_local_transform(i),
                );
                let p = &c.vehicle.mesh_props[i];
                let def = &c.vehicle.ty.model.meshes[c.vehicle.ty.meshes[i].def_index];
                let vp = def.viewpoint;
                let vp_ok = vp == 0 || vp & 4 != 0;
                // AI vehicles do not run every cockpit/material script that the player
                // vehicle runs.  Some models consequently leave an `[alphascale]`
                // variable at zero; applying it to an opaque body makes the traffic bus
                // translucent and reveals the interior through its panels.  Opaque slots
                // are never allowed to be faded by a dynamic alpha value; windows and
                // explicitly alpha-tested/blended slots retain their authored behavior.
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
                renderer.set_params(scene, *inst, &alpha, p.visible && vp_ok, &p.slot_uv);
                renderer.set_slot_light(scene, *inst, &p.slot_light);
                renderer.set_slot_night(scene, *inst, &p.slot_night);
                renderer.set_interior(scene, *inst, p.interior);
            }
            for (t, r) in c.vehicle.trailers.iter().zip(&c.trailer_renders) {
                for (i, inst) in r.instances.iter().enumerate() {
                    renderer.set_transform(scene, *inst, t.position, t.mesh_local_transform(i));
                    let p = &t.mesh_props[i];
                    let def = &t.ty.model.meshes[t.ty.meshes[i].def_index];
                    let vp = def.viewpoint;
                    let vp_ok = vp == 0 || vp & 4 != 0;
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
                    renderer.set_params(scene, *inst, &alpha, p.visible && vp_ok, &p.slot_uv);
                    renderer.set_slot_light(scene, *inst, &p.slot_light);
                    renderer.set_slot_night(scene, *inst, &p.slot_night);
                    renderer.set_interior(scene, *inst, p.interior);
                }
            }
        }
    }
}

impl Traffic {
    /// Take in what the tiles loaded since the last call brought: their lanes (linked into
    /// the network, whose existing indices stay valid), their parked cars (sorted onto the
    /// lanes once those are in) and the light programs of their crossings.
    pub fn add_tiles(&mut self, world: &World) -> usize {
        let (new, parked_cars, tiles) = take_from_tiles(world);
        let n = new.len();
        let mut added = self.net.lanes.len()..self.net.lanes.len();
        if n > 0 {
            added = self.net.extend(new, 1.5);
            self.street_weight += self.net.lanes[added.clone()].iter().filter_map(street_lane_weight).sum::<f64>();
            log::debug!(
                "traffic: {} lanes added ({} in all)",
                added.len(),
                self.net.lanes.len()
            );
        }
        if !tiles.is_empty() {
            self.lane_tiles.extend(tiles);
            self.lanes_generation += 1;
        }
        self.sort_parked(parked_cars, added);
        // new crossings bring their light programs; the running ones keep their clocks
        // (a program is never taken away again: the world's list only grows)
        let lights = world.traffic_lights.lock();
        if lights.len() > self.lights.len() {
            let from = self.lights.len();
            self.lights.extend(lights[from..].iter().cloned());
            self.light_prev
                .extend(lights[from..].iter().map(|c| vec![-100; c.lights.len()]));
            self.controller_of_object = world.controller_of_object.lock().clone();
        }
        n
    }
}

/// What the tiles placed since the last call hand to the traffic: their lanes, their parked
/// cars and which tiles they were (taken together, see `World::lane_tiles`).
fn take_from_tiles(
    world: &World,
) -> (
    Vec<omsi_sim::traffic::Lane>,
    Vec<(DVec3, f64)>,
    Vec<(i32, i32)>,
) {
    let mut lanes = world.lanes.lock();
    let parked = std::mem::take(&mut *world.parked_cars.lock());
    let tiles = std::mem::take(&mut *world.lane_tiles.lock());
    let mut new = std::mem::take(&mut *lanes);
    // The tiles are read in parallel and hand in their lanes in the order they finish:
    // sorted by their map identity, the lanes a set of tiles brings are numbered alike in
    // every run, and so is the random traffic drawn from them (a run can be repeated to
    // look at what a car did).
    new.sort_by(|a, b| {
        let first = |l: &omsi_sim::traffic::Lane| {
            l.points
                .first()
                .map(|p| (p.x.to_bits(), p.y.to_bits()))
                .unwrap_or((0, 0))
        };
        (
            a.key.map(|k| (k.tile, k.id, k.path)),
            a.reversed,
            a.source,
            first(a),
        )
            .cmp(&(
                b.key.map(|k| (k.tile, k.id, k.path)),
                b.reversed,
                b.source,
                first(b),
            ))
    });
    (new, parked, tiles)
}

// ---------------------------------------------------------------------------------------
// LAN play (see `lan_world`): a host keeps traffic around every player and tells the
// clients its light programs; a client draws the host's cars instead of its own.

impl Traffic {
    /// Street traffic around the other players of a LAN session too (host): each of them
    /// gets its own share of cars where no other player's share lies already.
    fn populate_lan_centers(
        &mut self,
        world: &World,
        renderer: &Renderer,
        scene: &mut Scene,
        center: DVec3,
        target: usize,
    ) {
        let centers = self.lan_centers.clone();
        let mut done = vec![center];
        for c in centers {
            if done
                .iter()
                .any(|d| (*d - c).truncate().length() < self.spawn_radius)
            {
                continue;
            }
            self.count_near = Some((c, self.spawn_radius));
            self.populate_kind(world, renderer, scene, c, LaneKind::Street, target);
            self.count_near = None;
            done.push(c);
        }
    }

    pub fn is_mirror(&self) -> bool {
        self.mirror
    }

    /// Draw the host's traffic from now on (`on`), or simulate our own again: either way
    /// every car there is now goes (ours make room for the host's, the host's copies
    /// cannot drive on by themselves).
    pub fn set_mirror(&mut self, world: &World, renderer: &Renderer, scene: &mut Scene, on: bool) {
        if self.mirror == on {
            return;
        }
        self.mirror = on;
        let ids: Vec<u64> = self.cars.iter().map(|c| c.id).collect();
        for id in ids {
            self.remove_car(world, renderer, scene, id);
        }
        self.initial = !on;
        log::info!(
            "traffic: {}",
            if on {
                "the LAN host's traffic is drawn instead of our own"
            } else {
                "simulating our own traffic again"
            }
        );
    }

    /// A client's frame: the clock and the light programs run on (the host corrects them
    /// every second, `set_light_state`); the cars are moved by `lan_world`.
    fn mirror_tick(&mut self, dt: f32) {
        self.time += dt;
        self.day_time += dt as f64 * self.time_scale;
        self.last_dt = dt;
        let day_time = self.day_time;
        for c in self.lights.iter_mut() {
            c.request.iter_mut().for_each(|r| *r = false);
            c.start(day_time);
            c.advance(dt);
        }
        self.log_lights();
    }

    /// A car of the host's traffic, standing at `pos` (client). Its id is the host's.
    #[allow(clippy::too_many_arguments)]
    pub fn add_mirror_car(
        &mut self,
        world: &World,
        renderer: &Renderer,
        scene: &mut Scene,
        id: u64,
        ty: Arc<VehicleType>,
        scheme: Option<usize>,
        scheduled: bool,
        pos: DVec3,
        heading: f64,
    ) -> usize {
        let mut host = omsi_sim::VehicleHost::new(omsi_sim::SimClock::default());
        host.font_lib = Some(world.fonts.clone());
        let scheme = scheme.filter(|i| *i < ty.paint_schemes.len());
        host.paint_scheme = Some(scheme);
        let mut vehicle = VehicleInstance::new(ty.clone(), host);
        // (the host's poses say where it stands; nothing here pulls it onto the ground)
        vehicle.ground = None;
        vehicle.apply_paint_vars(scheme);
        let render = world.add_vehicle_shared(renderer, scene, &ty, scheme, None);
        let trailer_renders =
            self.attach_trailers(world, renderer, scene, &mut vehicle, scheme, &render);
        if !ty.model.text_textures.is_empty() {
            vehicle.init_text_textures(&mut world.fonts.lock(), &|p| {
                omsi_texture::decode_file(p)
                    .ok()
                    .map(|i| (i.width, i.height, i.rgba))
            });
        }
        vehicle.position = pos;
        vehicle.heading = heading;
        let (front, rear, half_width) = extents(&ty, 4.5);
        let mut state = AiState::new(0, 0.0, id);
        state.front = front;
        state.rear = rear;
        state.length = front + rear;
        let body = AiBody::new(&ty.def, MotionKind::Road);
        self.cars.push(AiCar {
            id,
            state,
            vehicle,
            render,
            trailer_renders,
            body,
            stopped: 0.0,
            lead_car: None,
            ignore_lead: None,
            crawl: 0.0,
            progress: (0.0, 0.0),
            bus: scheduled.then(|| Box::new(BusService::new(Vec::new()))),
            sounds: None,
            half_width,
            yielding: false,
            light_hold: false,
            reserved: Vec::new(),
            amber: None,
            passing: None,
            gone: false,
            fresh: 0.0,
            merge_after: None,
            holding: None,
            why: ("", 0.0),
            held: false,
            geo_block: None,
            lead_info: None,
            junction_why: String::new(),
            wait_at: None,
            seed: 0,
            scheme: None,
            squeeze: None,
            pass_room: 0.0,
            pass_retry: 0.0,
            light_at: None,
            pull_out: 0.0,
            rail_trail: Default::default(),
            consist_reversed: false,
            park: None,
        });
        self.cars.len() - 1
    }

    /// A vehicle type the traffic has loaded already (the random traffic's types and the
    /// coupled parts), by file.
    pub fn loaded_type(&self, path: &Path) -> Option<Arc<VehicleType>> {
        self.types
            .iter()
            .map(|t| &t.0)
            .chain(self.trailer_types.values().flatten())
            .find(|t| t.def.path == path)
            .cloned()
    }

    /// The light programs of the crossings within `radius` of `near` (host): (crossing
    /// object, position in the cycle, clock held at a stop point).
    pub fn light_states(&self, near: DVec3, radius: f64) -> Vec<(i64, f64, bool)> {
        let mut ctls: Vec<usize> = self
            .net
            .lanes
            .iter()
            .filter(|l| {
                l.traffic_light.is_some()
                    && l.points
                        .first()
                        .map(|p| (*p - near).truncate().length() < radius)
                        .unwrap_or(false)
            })
            .filter_map(|l| l.traffic_light.map(|t| t.0))
            .collect();
        ctls.sort_unstable();
        ctls.dedup();
        self.controller_of_object
            .iter()
            .filter(|(_, c)| ctls.binary_search(c).is_ok())
            .filter_map(|(obj, c)| {
                let ctl = self.lights.get(*c)?;
                Some((*obj, ctl.time, ctl.held))
            })
            .collect()
    }

    /// Where the host's light program of crossing `object` stands (client).
    pub fn set_light_state(&mut self, object: i64, time: f64, held: bool) {
        if let Some(ctl) = self
            .controller_of_object
            .get(&object)
            .and_then(|c| self.lights.get_mut(*c))
        {
            ctl.time = time;
            ctl.held = held;
        }
    }
}

/// How many times the base street target the neighbourhood asks for, from the trafficdensity
/// of each street lane starting near the player (0 for a no_cars lane): the count of lanes
/// per about 250 (1 to 4) times their mean density (0 to 2).
fn road_scale(near_density: &[f32]) -> f32 {
    let road = (near_density.len() as f32 / 250.0).clamp(1.0, 4.0);
    if near_density.is_empty() {
        return road;
    }
    let mean = near_density.iter().sum::<f32>() / near_density.len() as f32;
    road * mean.clamp(0.0, 2.0)
}

/// Whether the point `p` of a car's way lies in the player's box round `centre` (`wide` to
/// either side, `ahead` in front and `behind` behind it) - on the same level only: a bus
/// under a bridge held up the traffic on the bridge above it (#753). 4 m, as for the other
/// vehicles' bodies.
fn in_player_box(p: DVec3, centre: DVec3, fwd: DVec2, right: DVec2, wide: f64, ahead: f64, behind: f64) -> bool {
    let rel = p.truncate() - centre.truncate();
    let (x, y) = (rel.dot(right), rel.dot(fwd));
    x.abs() <= wide && y <= ahead && y >= -behind && (p.z - centre.z).abs() < 4.0
}

#[cfg(test)]
mod road_scale_tests {
    use super::road_scale;

    #[test]
    fn path_density_scales_the_street_target() {
        assert!((road_scale(&[1.0; 100]) - 1.0).abs() < 1e-6);
        assert!((road_scale(&[0.2; 100]) - 0.2).abs() < 1e-6);
        assert_eq!(road_scale(&[0.0; 100]), 0.0);
        assert!((road_scale(&[1.0; 500]) - 2.0).abs() < 1e-6);
    }
}

#[cfg(test)]
mod junction_arrival_tests {
    use super::crossing_arrival;
    use omsi_sim::traffic::AiState;

    #[test]
    fn stopped_queue_does_not_predict_a_restart() {
        let st = AiState::new(0, 0.0, 1);
        assert_eq!(crossing_arrival(&st, 10.0, false, false, true), f32::MAX);
    }

    #[test]
    fn crawling_queue_is_measured_at_its_actual_speed() {
        let mut st = AiState::new(0, 0.0, 1);
        for speed in [0.2, 0.3, 0.4, 0.5, 0.8, 1.4] {
            st.speed = speed;
            assert!((crossing_arrival(&st, 10.0, false, false, true) - 10.0 / speed).abs() < 1e-5);
        }
    }

    #[test]
    fn crawling_queue_allows_a_gap_but_nearby_traffic_still_counts() {
        let mut st = AiState::new(0, 0.0, 1);
        st.speed = 0.4;
        let gap = 7.5;
        assert!(crossing_arrival(&st, 10.0, false, false, true) > gap);
        assert!(crossing_arrival(&st, 1.0, false, false, true) < gap);
        // Once it can move freely again, account for it accelerating towards the crossing.
        assert!(crossing_arrival(&st, 10.0, true, false, false) < gap);
    }

    #[test]
    fn queue_already_at_the_conflict_still_blocks() {
        let st = AiState::new(0, 0.0, 1);
        for distance in [-1.0, 0.0, 0.3] {
            assert_eq!(crossing_arrival(&st, distance, false, true, true), 0.0);
        }
    }

    #[test]
    fn freely_starting_car_keeps_its_accelerating_prediction() {
        let st = AiState::new(0, 0.0, 1);
        let expected = (20.0 / st.accel).sqrt() + st.reaction;
        assert!((crossing_arrival(&st, 10.0, false, false, false) - expected).abs() < 1e-5);
        assert!((crossing_arrival(&st, 10.0, true, false, false) - expected).abs() < 1e-5);
    }

    #[test]
    fn car_waiting_before_the_conflict_is_not_approaching() {
        let mut st = AiState::new(0, 0.0, 1);
        st.speed = 2.0;
        assert_eq!(crossing_arrival(&st, 10.0, false, true, false), f32::MAX);
        assert!(crossing_arrival(&st, 10.0, true, false, false) < 5.0);
        assert_eq!(crossing_arrival(&st, 10.0, false, false, false), 5.0);
    }
}

#[cfg(test)]
mod group_density_tests {
    use super::player_reach_ahead;
    use omsi_sim::traffic::pool_density as uvg_density;
    use glam::DVec2;

    /// Berlin-Spandau's `unsched_vehgroups.txt`: NormalCars 1, Trucks 0, Commercials 1,
    /// Ambulance 1, GDRCars 0.
    const SPANDAU: [i32; 5] = [1, 0, 1, 1, 0];

    #[test]
    fn a_bus_under_a_bridge_is_not_in_the_way_on_it() {
        use super::in_player_box;
        use glam::DVec3;
        let (c, f, r) = (DVec3::new(0.0, 0.0, 32.0), DVec2::new(0.0, 1.0), DVec2::new(1.0, 0.0));
        // the road through the bus's box, on its level and on a bridge 5.4 m above it
        assert!(in_player_box(DVec3::new(0.5, 3.0, 32.3), c, f, r, 2.5, 6.0, 6.0));
        assert!(!in_player_box(DVec3::new(0.5, 3.0, 37.4), c, f, r, 2.5, 6.0, 6.0));
        assert!(!in_player_box(DVec3::new(0.5, 3.0, 26.0), c, f, r, 2.5, 6.0, 6.0));
        // beside it
        assert!(!in_player_box(DVec3::new(3.5, 3.0, 32.0), c, f, r, 2.5, 6.0, 6.0));
    }

    #[test]
    fn a_following_bus_is_no_bus_in_the_way() {
        let north = DVec2::new(0.0, 1.0);
        // a car ahead going the same way: only the bus itself counts
        assert_eq!(player_reach_ahead(6.0, 14.0, 1.5, north, DVec2::new(0.1, 3.0)), 6.0);
        // a car crossing its way (or coming towards it): where the bus will be counts too
        assert_eq!(player_reach_ahead(6.0, 14.0, 1.5, north, DVec2::new(3.0, 0.0)), 27.0);
        assert_eq!(player_reach_ahead(6.0, 14.0, 1.5, north, DVec2::new(0.0, -3.0)), 27.0);
    }

    #[test]
    fn a_group_off_by_default_drives_where_a_path_asks_for_it() {
        // a Falkensee path: no rule for the normal cars, the GDR cars asked for
        let rules = [(4u16, 1.0f32)];
        assert_eq!(uvg_density(&rules, &SPANDAU, 4), 1.0);
        assert_eq!(uvg_density(&rules, &SPANDAU, 0), 1.0);
        // and nowhere else
        assert_eq!(uvg_density(&[], &SPANDAU, 4), 0.0);
        assert_eq!(uvg_density(&[(0, 0.5)], &SPANDAU, 1), 0.0);
    }

    #[test]
    fn a_default_follows_the_first_group_on_the_path() {
        // commercials (default 1) take the normal cars' density of the path
        assert_eq!(uvg_density(&[(0, 0.4)], &SPANDAU, 2), 0.4);
        assert_eq!(uvg_density(&[(0, 0.0)], &SPANDAU, 2), 0.0);
        assert_eq!(uvg_density(&[], &SPANDAU, 2), 1.0);
        // an own rule wins
        assert_eq!(uvg_density(&[(0, 0.4), (2, 2.0)], &SPANDAU, 2), 2.0);
    }

    #[test]
    fn defaults_naming_each_other_end() {
        assert_eq!(uvg_density(&[], &[1, 3, 2], 1), 0.0);
    }
}


#[cfg(test)]
mod way_user_tests {
    use super::*;
    use omsi_sim::traffic::{Crossing, LaneBuilder};

    fn street(start: DVec3, heading: f64, length: f64, radius: f64) -> omsi_sim::traffic::Lane {
        LaneBuilder::arc(start, heading, length, radius, 0.0, LaneKind::Street, 3.0)
    }

    /// A road north (lane 0) forking into straight on (1) and a left turn (2).
    fn fork() -> Network {
        let a = street(DVec3::ZERO, 0.0, 50.0, 0.0);
        let b = street(a.end(), 0.0, 20.0, 0.0);
        let mut c = street(a.end(), 0.0, 15.7, -10.0);
        c.turn = 1;
        let mut net = Network { lanes: vec![a, b, c], ..Default::default() };
        net.link(1.5);
        net.build_grid();
        net
    }

    #[test]
    fn the_players_bus_is_put_onto_the_lanes_it_may_take() {
        let net = fork();
        let bus: PlayerBox = (DVec3::new(0.0, 20.0, 0.0), 0.0, 6.0, 1.25, 10.0);
        let u = way_user_on(&net, &bus, 0, 0.0, false).expect("on the road");
        let lanes: Vec<usize> = u.lanes.iter().map(|l| l.0).collect();
        assert_eq!(lanes[0], 0);
        assert!((u.lanes[0].1 + 20.0).abs() < 0.1, "{:?}", u.lanes);
        // no indicator: either way (the cars cannot know)
        assert!(lanes.contains(&1) && lanes.contains(&2), "{lanes:?}");
        assert!(u.lanes.iter().filter(|l| l.0 != 0).all(|l| (l.1 - 30.0).abs() < 0.1), "{:?}", u.lanes);
        // indicating left: the left turn only; right (no such branch): both
        let left: Vec<usize> = way_user_on(&net, &bus, 1, 0.0, false).unwrap().lanes.iter().map(|l| l.0).collect();
        assert_eq!(left, vec![0, 2]);
        assert_eq!(way_user_on(&net, &bus, 2, 0.0, false).unwrap().lanes.len(), 3);
        // reversing, or off the road: not on the lanes
        assert!(way_user_on(&net, &(bus.0, 0.0, 6.0, 1.25, -2.0), 0, 0.0, false).is_none());
        assert!(way_user_on(&net, &(DVec3::new(30.0, 20.0, 0.0), 0.0, 6.0, 1.25, 5.0), 0, 0.0, false).is_none());
    }

    fn user(lanes: Vec<(usize, f32)>, speed: f32, still: f32) -> WayUser {
        WayUser { lanes, speed, half_len: 6.0, still, prio: false }
    }

    /// A side road's car 15 m before the meeting place with the main road the bus drives on.
    fn side_road_car(speed: f32) -> AiState {
        let mut st = AiState::new(0, 0.0, 1);
        st.speed = speed;
        st.accept_gap = 5.0;
        st
    }

    #[test]
    fn a_car_on_the_side_road_gives_way_to_the_players_bus() {
        let c = Crossing { other: 1, at: 2.0, other_at: 2.0, merge: false, before: 1.5, after: 1.5, other_before: 1.5, other_after: 1.5 };
        let st = side_road_car(5.0);
        let point = 17.5; // the car's origin to the meeting point
        // the bus 50 m off on the main road at 11 m/s (there in about four seconds)
        let coming = user(vec![(7, -10.0), (1, 40.0)], 11.0, 0.0);
        assert_eq!(way_user_verdict(&st, &coming, 40.0, &c, point, false, false, false, true, 1.0).0, Verdict::Ruled);
        // the car has the right of way, or has claimed the junction already: it goes
        assert_eq!(way_user_verdict(&st, &coming, 40.0, &c, point, false, false, false, false, 1.0).0, Verdict::Free);
        assert_eq!(way_user_verdict(&st, &coming, 40.0, &c, point, false, true, false, true, 1.0).0, Verdict::Free);
        // far off (a gap any driver takes), or standing at a stop: it goes
        let far = user(vec![(1, 140.0)], 11.0, 0.0);
        assert_eq!(way_user_verdict(&st, &far, 140.0, &c, point, false, false, false, true, 1.0).0, Verdict::Free);
        let standing = user(vec![(1, 20.0)], 0.0, 30.0);
        assert_eq!(way_user_verdict(&st, &standing, 20.0, &c, point, false, false, false, true, 1.0).0, Verdict::Free);
        // in the meeting place: whoever has the right of way, the car waits
        let there = user(vec![(1, -6.0)], 0.0, 30.0);
        assert_eq!(way_user_verdict(&st, &there, -6.0, &c, point, false, false, false, false, 1.0).0, Verdict::Hard);
        // in the junction and on its way through, there first: the car waits
        let crossing = user(vec![(1, -1.0)], 8.0, 0.0);
        assert_eq!(way_user_verdict(&st, &crossing, -1.0, &c, 4.0, false, false, false, false, 1.0).0, Verdict::Hard);
        // through already
        let through = user(vec![(1, -20.0)], 8.0, 0.0);
        assert_eq!(way_user_verdict(&st, &through, -20.0, &c, point, false, false, false, true, 1.0).0, Verdict::Free);
    }

    /// Two lanes running into one (lane 0 from the south, lane 1 slanting in from the
    /// south-east) and on as lane 2.
    fn joint() -> Network {
        let main = LaneBuilder::polyline(vec![DVec3::ZERO, DVec3::new(0.0, 50.0, 0.0)], LaneKind::Street, 3.0);
        let side = LaneBuilder::polyline(vec![DVec3::new(30.0, 20.0, 0.0), DVec3::new(0.0, 50.0, 0.0)], LaneKind::Street, 3.0);
        let on = LaneBuilder::polyline(vec![DVec3::new(0.0, 50.0, 0.0), DVec3::new(0.0, 120.0, 0.0)], LaneKind::Street, 3.0);
        let mut net = Network { lanes: vec![main, side, on], ..Default::default() };
        net.link(1.5);
        net.build_grid();
        net
    }

    #[test]
    fn a_car_merging_in_keeps_behind_the_players_bus() {
        let net = joint();
        let side_len = net.lanes[1].length();
        // the car 20 m before the joint at 8 m/s
        let mut me = AiState::new(1, side_len - 20.0, 1);
        me.speed = 8.0;
        me.planned_next = Some(2);
        // the bus 20 m before the joint at 12 m/s, its way on through it: there first
        let bus = user(vec![(0, -30.0), (2, 20.0)], 12.0, 0.0);
        let l = merging_lead(&net, &me, &[bus]).expect("the bus goes first");
        assert!(l.gap > 0.0 && l.gap < 20.0, "{l:?}");
        // the bus far back, or standing: the car goes
        assert!(merging_lead(&net, &me, &[user(vec![(0, 0.0), (2, 50.0)], 12.0, 0.0)]).is_none());
        assert!(merging_lead(&net, &me, &[user(vec![(0, -30.0), (2, 20.0)], 0.0, 9.0)]).is_none());
        // a bus whose way does not go on into the lane the car takes: nothing to do with it
        assert!(merging_lead(&net, &me, &[user(vec![(0, -30.0)], 12.0, 0.0)]).is_none());
    }
}
