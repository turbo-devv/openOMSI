//! People on foot: passengers and pedestrians as agents with a goal.
//!
//! A passenger comes along the pavement (or already stands at the stop when the map
//! starts), waits at a free waiting place of the stop - the `[passpos]` points of the
//! map's `people_standing_*` markers and shelters, else places spread along the back of
//! the platform - and when a bus opens its doors there, queues at the nearest open
//! `[entry]` (a passenger who still has to buy a ticket only at one with a cash desk),
//! steps in when the doorway is free, pays or shows a pass at the desk, walks the cabin's
//! `paths.cfg` network to a free `[passpos]` (a standing place once the seats are gone),
//! rides, presses the stop button before their stop, walks to the nearest `[exit]` when
//! the bus stands there, steps out and walks away along the pavement - or waits at the
//! stop for another bus. Timetable (AI) buses carry their passengers the same way.
//! Nobody is taken away while the player can see them.
//!
//! An articulated bus is one cabin: the sections' path networks, seats and exits are put
//! together in the front section's frame with the sections straight behind each other, and
//! the front section's `[linkToPrevVeh]` point is joined to the rear section's
//! `[linkToNextVeh]` point, so people walk through the bellows to the seats and exits at the
//! back. Entries and exits are numbered front section first, which is how the stock door
//! scripts count them (the GN92's rear door is `PAX_Exit2`/`PAX_Exit3`). A point behind a
//! joint is carried by its own section, whatever the angle of the bend.
//!
//! Movement is a crowd: everybody on the same floor (the ground, or one bus) avoids
//! everybody else with the anticipatory model of `omsi_sim::crowd`, does not push into
//! somebody standing in front, speeds up, slows down and turns at a human pace, and keeps
//! to the aisle inside a bus. Doorways and the cash desk are taken one at a time, people
//! getting off go first, and somebody pressed against another for seconds slips past.
//! Every waiting state has a way out, and `OMSI_DEBUG_PAX=1` logs every change of state
//! and why somebody stands still.
//!
//! Pedestrians walk the map's pavement paths as one network (path ends that meet are
//! joined whatever their heading), wait at the kerb for a pedestrian light's green - and
//! only start across when it lasts long enough - and for approaching cars where there is
//! no light; nobody stops in the middle of the road.
//!
//! The map streams: stops, waiting places and pavements come with their tiles. The
//! pavement network grows as the traffic network does, a stop is set up again when its
//! neighbourhood changed and nobody uses it, a stop whose tile went takes its people with
//! it, and nobody stands or walks where the ground is not loaded.

use crate::ambience;
use crate::scene::World;
use crate::traffic::Traffic;
use glam::{DVec2, DVec3, Mat4, Vec3};
use hashbrown::{HashMap, HashSet};
use omsi_render::{AlphaMode, Camera, MaterialId, MeshId, Renderer, Scene};
use omsi_sim::crowd::{self, Block, CrowdParams, PathGraph, Walker};
use omsi_sim::human::{skin, Activity, HumanType};
use omsi_sim::human_omsi::{AnimInput, OmsiAnim};
use omsi_sim::traffic::{LaneKind, Network};
use omsi_sim::VehicleInstance;
use omsi_vehicle::PassengerCabin;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[path = "humans_pax.rs"]
mod pax;
use pax::*;

/// The map's traffic keeps left (its stops are on the left): see the doors of `Cabin`.
pub(crate) static LEFT_HAND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// How far outside the bus side somebody stands at a door (m).
const DOOR_OUT: f32 = 0.5;
/// Body radius for the crowd outside (m): shoulders and swinging arms. With the cabin's
/// radius people on the pavement came within 0.46 m, and two walking past each other or a
/// group crossing the road merged into one another in the picture.
const BODY_OUTSIDE: f64 = 0.28;
/// Stops within this distance of the player have their people (Omsi.exe: the stop's tile
/// and the eight round the camera's, sub_61bf94).
const STOP_RANGE: f64 = 450.0;
/// Pedestrians stroll within this distance of the player (m).
const STROLL_RADIUS: f64 = 200.0;
/// How far in front of a seat's hip point somebody stands to sit down - where the feet
/// stay while seated (m).
const SEAT_FRONT: f32 = 0.34;
/// Over this distance on either side of a joint (m) a point of an articulated bus's cabin
/// moves from the frame of the section in front to the one behind.
const JOINT_BLEND: f32 = 0.5;

fn debug_pax() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        omsi_cfg::env::var_os("OMSI_DEBUG_PAX").is_some()
            || omsi_cfg::env::var_os("OMSI_DEBUG_HUMANS").is_some()
    })
}


/// Where the player looks from, for "nobody appears or vanishes in sight".
#[derive(Debug, Clone, Copy)]
pub struct Eye {
    pub pos: DVec3,
    pub fwd: DVec3,
    /// Cosine of half the diagonal field of view, with a margin.
    pub cos_half: f64,
}

impl Eye {
    pub fn of(cam: &Camera, aspect: f32) -> Eye {
        let half_v = (cam.fov_deg as f64 * 0.5).to_radians();
        let half_diag = (half_v.tan() * (1.0 + (aspect as f64).powi(2)).sqrt()).atan();
        Eye {
            pos: cam.position,
            fwd: cam.forward().as_dvec3().normalize_or_zero(),
            cos_half: (half_diag + 10f64.to_radians())
                .min(89f64.to_radians())
                .cos(),
        }
    }

    /// A wider picture than the camera's own (a triple screen's side panels): the tangents
    /// of its half-angles, horizontal and vertical.
    pub fn widened(mut self, extent: Option<(f64, f64)>) -> Eye {
        if let Some((tan_x, tan_y)) = extent {
            let half_diag = tan_x.hypot(tan_y).atan();
            self.cos_half = self.cos_half.min((half_diag + 10f64.to_radians()).min(89f64.to_radians()).cos());
        }
        self
    }
}

/// A bus as the passengers know it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BusId {
    Player,
    Ai(u64),
}

impl BusId {
}

/// An `[entry]` or `[exit]` of a cabin, in the bus frame.
#[derive(Debug, Clone)]
struct Door {
    /// The door's path point (the threshold) and its index.
    inside: Vec3,
    point: Option<usize>,
    /// Where somebody stands just outside, at ground level.
    outside: Vec3,
    /// +1 on the right side of the bus, -1 on the left.
    side: f32,
    /// Direction along the bus (+1 forwards) in which the queue at this door runs.
    queue_dir: f32,
    /// A passenger who still has to buy a ticket may board here (no `{noticketsale}`).
    sells: bool,
    /// `{withbutton}`: a door the passenger opens with the request button, worth walking to
    /// while it is still shut.
    button: bool,
    /// Where people getting off wait for the door to open: the path point next to it.
    wait: Vec3,
}

impl Door {

}

#[derive(Debug, Clone)]
struct Seat {
    /// The `[passpos]` point: a seated passenger's hip, a standing one's feet.
    pos: Vec3,
    /// The floor in front of it, where the feet go (and where a seated passenger stands
    /// before sitting down and after getting up).
    floor: Vec3,
    rot: f32,
    seated: bool,
    /// The `[passpos]`'s seat height (+0x20; 0: a standing place).
    height: f32,
    /// Its number for the scripts (`GetHumanCountOnSeat`): Omsi.exe's place in the file
    /// among the `[passpos]` and `[drivpos]`, the sections behind counted on after those
    /// in front (0x7d39a4 asks the next one for a number past its own places).
    omsi_seat: usize,
}

/// What passengers need to know about one vehicle type's cabin.
struct Cabin {
    data: PassengerCabin,
    graph: PathGraph,
    links: Vec<(i32, i32, bool)>,
    /// Each link's footstep sounds: its section's `[stepsoundpack]` named by the link's
    /// `[next_stepsound]` (index into `step_packs`), none where the paths.cfg gives none -
    /// Omsi.exe hears no steps there - and on the joint between two sections.
    link_pack: Vec<Option<usize>>,
    step_packs: Vec<Arc<[String]>>,
    entries: Vec<Door>,
    exits: Vec<Door>,
    /// Where a passenger stands at the cash desk, its path point, and the heading (bus
    /// frame) they face: between the desk top, where the money goes, and the driver.
    desk: Option<(Vec3, Option<usize>, f64)>,
    seats: Vec<Seat>,
    /// The sections (one for a rigid bus), front first; everything above is in the
    /// unfolded frame of the front section.
    parts: Vec<CabinPart>,
    /// Each link's room height (`[next_roomheight]`; 2 m before any).
    link_room: Vec<f32>,
    /// The routing tables of the path network (sub_72410c).
    routes: Vec<Vec<RouteLink>>,
    /// The validator and the cash desk as Omsi.exe keeps them - one each, the last of the
    /// file (cabin +0x14/+0x18, +0x28/+0x2c): (path point, device).
    stamper: Option<(Option<usize>, Vec3)>,
    sale: Option<(Option<usize>, Vec3)>,
    /// Where the money goes (+0x38) and where the change is taken from (+0x58), with the
    /// money point's spread.
    money_point: Option<Vec3>,
    money_var: Option<(Vec3, [f32; 2])>,
    change_point: Option<Vec3>,
}

/// The people on each seat by the scripts' numbers (`Seat::omsi_seat`, the `[drivpos]`
/// counted with the `[passpos]`), from the places (indices into `seats`) taken by people
/// sitting there. (Counted by the `[passpos]` alone, every seat of a cabin with the
/// driver's place first was one off: a tip-up seat folded down under the next one.)
fn seat_numbers(seats: &[Seat], sitting: impl Iterator<Item = usize>) -> Vec<u32> {
    let n = seats.iter().map(|s| s.omsi_seat + 1).max().unwrap_or(0);
    let mut out = vec![0u32; n];
    for k in sitting {
        if let Some(c) = seats.get(k).and_then(|s| out.get_mut(s.omsi_seat)) {
            *c += 1;
        }
    }
    out
}

/// A section of an articulated bus in its cabin's unfolded frame.
#[derive(Debug, Clone, Copy)]
struct CabinPart {
    /// Where the section's own origin lies.
    offset: Vec3,
    /// The unfolded y of the joint in front of it (the front section: none, +inf).
    joint_y: f32,
}

/// One vehicle of a coupled train as a cabin is put together from it: its definition, its
/// origin in the front vehicle's unfolded frame, and the unfolded y of its front joint.
type TrainPart<'a> = (&'a omsi_vehicle::Vehicle, Vec3, f32);

/// The sections of `v` passengers can walk through, front first: the vehicle and every
/// coupled part straight behind it (a part coupled the wrong way round and all behind it
/// are left out).
fn train_parts(v: &VehicleInstance) -> Vec<TrainPart<'_>> {
    let mut out: Vec<TrainPart<'_>> = vec![(&v.ty.def, Vec3::ZERO, f32::INFINITY)];
    let mut offset = Vec3::ZERO;
    for t in &v.trailers {
        if t.reversed {
            break;
        }
        let (back, front) = t.couplings();
        let joint_y = offset.y + back.y;
        offset += back - front;
        out.push((&t.ty.def, offset, joint_y));
    }
    out
}

impl Cabin {
    /// The cabin of a train of vehicles (see [`train_parts`]): the front one's, with the
    /// sections behind joined on as far as they have a cabin and a path network.
    fn load_train(parts: &[TrainPart<'_>]) -> Option<Cabin> {
        let (lead, _, _) = parts.first()?;
        let load_cabin = |def: &omsi_vehicle::Vehicle| -> Option<PassengerCabin> {
            let rel = def.passenger_cabin.as_ref()?;
            PassengerCabin::load(&omsi_cfg::resolve_path(def.dir(), rel))
                .map_err(|e| log::warn!("{e}"))
                .ok()
        };
        let load_paths = |def: &omsi_vehicle::Vehicle| {
            def.paths.as_ref().and_then(|rel| {
                omsi_vehicle::VehiclePaths::load(&omsi_cfg::resolve_path(def.dir(), rel))
                    .map_err(|e| log::warn!("{e}"))
                    .ok()
            })
        };
        let data = load_cabin(lead)?;
        let mut points: Vec<Vec3> = Vec::new();
        let mut links: Vec<(i32, i32, bool)> = Vec::new();
        let mut link_pack: Vec<Option<usize>> = Vec::new();
        let mut link_room: Vec<f32> = Vec::new();
        let mut step_packs: Vec<Arc<[String]>> = Vec::new();
        // (merged path point or -1, sells tickets, {withbutton}, half width of the section)
        let mut entry_points: Vec<(i32, bool, bool, f32)> = Vec::new();
        let mut exit_points: Vec<(i32, f32)> = Vec::new();
        let mut places: Vec<(omsi_vehicle::cabin::PassPos, Vec3, usize)> = Vec::new();
        // (the script seat numbers of the sections in front)
        let mut seat_base = 0usize;
        let mut cabin_parts: Vec<CabinPart> = Vec::new();
        // the point of the section in front that leads on to the next one
        let mut rear_link: Option<usize> = None;
        for (k, (def, offset, joint_y)) in parts.iter().enumerate() {
            let cab = if k == 0 {
                Some(data.clone())
            } else {
                load_cabin(def)
            };
            let Some(cab) = cab else { break };
            let (own, own_links, own_steps, own_packs, own_rooms): (Vec<Vec3>, Vec<(i32, i32, bool)>, Vec<i32>, Vec<Vec<String>>, Vec<f32>) = match load_paths(def) {
                Some(p) => (
                    p.points
                        .iter()
                        .map(|q| Vec3::from(q.pos) + *offset)
                        .collect(),
                    p.links,
                    p.link_step_sound,
                    p.step_sound_packs,
                    p.link_room_height,
                ),
                None => (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()),
            };
            let base = points.len();
            let valid = |i: i32| (i >= 0 && (i as usize) < own.len()).then_some(base + i as usize);
            let end = |front: bool| {
                (0..own.len())
                    .filter(|i| own[*i].x.abs() < 0.6)
                    .max_by(|a, b| {
                        if front {
                            own[*a].y.total_cmp(&own[*b].y)
                        } else {
                            own[*b].y.total_cmp(&own[*a].y)
                        }
                    })
                    .map(|i| base + i)
            };
            if k > 0 {
                // through the joint: from the front section's [linkToPrevVeh] point to this
                // one's [linkToNextVeh] point (the frontmost aisle point when it has none)
                let front = cab.link_to_next_veh.and_then(valid).or_else(|| end(true));
                match (rear_link, front) {
                    (Some(a), Some(b)) => {
                        links.push((a as i32, b as i32, false));
                        link_pack.push(None);
                        link_room.push(2.0);
                    }
                    // no way through: the section stays empty
                    _ => break,
                }
            }
            points.extend(own.iter().copied());
            links.extend(
                own_links
                    .iter()
                    .map(|(a, b, o)| (a + base as i32, b + base as i32, *o)),
            );
            let pack_base = step_packs.len();
            link_pack.extend((0..own_links.len()).map(|i| {
                let n = own_steps.get(i).copied().unwrap_or(-1);
                (n >= 0 && (n as usize) < own_packs.len()).then(|| pack_base + n as usize)
            }));
            step_packs.extend(own_packs.into_iter().map(Arc::from));
            link_room.extend((0..own_links.len()).map(|i| own_rooms.get(i).copied().unwrap_or(2.0)));
            rear_link = cab.link_to_prev_veh.and_then(valid).or_else(|| end(false));
            let half = def
                .bounding_box
                .map(|b| b[0] * 0.5)
                .unwrap_or_else(|| own.iter().map(|p| p.x.abs()).fold(1.2, f32::max));
            let shift = |i: i32| valid(i).map(|m| m as i32).unwrap_or(-1);
            entry_points.extend(
                cab.entries
                    .iter()
                    .map(|e| (shift(e.path_point), !e.no_ticket_sale, e.with_button, half)),
            );
            exit_points.extend(cab.exits.iter().map(|e| (shift(*e), half)));
            places.extend(cab.pass_positions.iter().map(|p| (p.clone(), *offset, seat_base + p.file_index)));
            seat_base += cab.pass_positions.len() + cab.driver_positions.len();
            cabin_parts.push(CabinPart {
                offset: *offset,
                joint_y: *joint_y,
            });
        }
        let graph = PathGraph::new(points.clone(), &links);
        // (the side of the road the stops are on: where a door's own point does not tell)
        let kerb = if LEFT_HAND.load(std::sync::atomic::Ordering::Relaxed) { -1.0f32 } else { 1.0 };
        let door = |pp: i32, sells: bool, button: bool, half_width: f32| -> Door {
            let point = (pp >= 0 && (pp as usize) < points.len()).then_some(pp as usize);
            let inside = point
                .map(|i| points[i])
                .unwrap_or(Vec3::new(kerb * (half_width - 0.1), 4.0, 0.4));
            // A door's side is the side of its entry point; one in the middle of the aisle
            // (or none) is taken to open to the kerb - on the left where the traffic keeps
            // left. (Always the right: a UK bus whose entry point lies on the aisle had the
            // people come to its door from the road side, round the bus.)
            let side = if inside.x.abs() < 0.6 { kerb } else if inside.x >= 0.0 { 1.0 } else { -1.0 };
            let outside = Vec3::new(side * (half_width + DOOR_OUT), inside.y, 0.0);
            // the aisle point next to the door: its neighbour nearest the middle
            let wait_point = point
                .and_then(|i| {
                    graph
                        .neighbours(i)
                        .into_iter()
                        .min_by(|a, b| points[*a].x.abs().total_cmp(&points[*b].x.abs()))
                })
                .filter(|&w| (points[w].x - inside.x).abs() > 0.3);
            // (no aisle point linked beside the door - the W906's door steps lead straight on
            // along it: the nearest path point off the door's line, else a step inwards)
            let wait = wait_point.map(|w| points[w]).unwrap_or_else(|| {
                points
                    .iter()
                    .filter(|p| (p.x - inside.x).abs() > 0.3 && (p.truncate() - inside.truncate()).length() < 1.2 && (p.z - inside.z).abs() < 0.6)
                    .min_by(|a, b| (a.truncate() - inside.truncate()).length().total_cmp(&(b.truncate() - inside.truncate()).length()))
                    .copied()
                    .unwrap_or(Vec3::new(inside.x - side * 0.7, inside.y, inside.z))
            });
            Door {
                inside,
                point,
                outside,
                side,
                queue_dir: -1.0,
                sells,
                button,
                wait,
            }
        };
        let mut entries: Vec<Door> = entry_points
            .iter()
            .map(|(pp, sells, button, half)| door(*pp, *sells, *button, *half))
            .collect();
        let exits: Vec<Door> = exit_points
            .iter()
            .map(|(pp, half)| door(*pp, false, false, *half))
            .collect();
        // two leaves of one door: the queue of the front leaf runs forwards, the other's back,
        // so that the two lines do not stand in each other
        for i in 0..entries.len() {
            let partner = (0..entries.len()).find(|&j| {
                j != i
                    && entries[j].side == entries[i].side
                    && (entries[j].inside.y - entries[i].inside.y).abs() < 1.4
            });
            entries[i].queue_dir = match partner {
                Some(j) if entries[j].inside.y < entries[i].inside.y => 1.0,
                _ => -1.0,
            };
        }
        let desk = data.ticket_sales.first().map(|ts| {
            let top = Vec3::from(ts.pos);
            let by_point = usize::try_from(ts.path_point)
                .ok()
                .and_then(|i| points.get(i).map(|p| (i, *p)))
                .filter(|(_, p)| (top.truncate() - p.truncate()).length() < 3.0);
            let (stand, pi) = match by_point {
                Some((i, p)) => (p, Some(i)),
                None => {
                    // no usable path point: the nearest one on the entry floor, else the floor by the desk
                    let floor = entries.first().map(|e| e.inside.z).unwrap_or(0.4);
                    let near = points
                        .iter()
                        .enumerate()
                        .filter(|(_, p)| (p.z - floor).abs() < 0.6)
                        .min_by(|a, b| {
                            (a.1.truncate() - top.truncate())
                                .length()
                                .total_cmp(&(b.1.truncate() - top.truncate()).length())
                        });
                    match near {
                        Some((i, p)) => (*p, Some(i)),
                        None => (Vec3::new(top.x + 0.4, top.y, floor), None),
                    }
                }
            };
            let target = match data.driver_positions.first() {
                Some(d) => (top + Vec3::from(d.pos)) * 0.5,
                None => top,
            };
            let d = target - stand;
            let face = if d.truncate().length() > 0.05 {
                (d.x as f64).atan2(d.y as f64).to_degrees()
            } else {
                -90.0
            };
            (stand, pi, face)
        });
        let seats = places
            .iter()
            .map(|(p, offset, omsi_seat)| {
                let pos = Vec3::from(p.pos) + *offset;
                let seated = p.height > 0.01;
                let floor = if seated {
                    let r = p.rot.to_radians();
                    Vec3::new(
                        pos.x + r.sin() * SEAT_FRONT,
                        pos.y + r.cos() * SEAT_FRONT,
                        pos.z - p.height,
                    )
                } else {
                    pos
                };
                Seat {
                    pos,
                    floor,
                    rot: p.rot,
                    seated,
                    height: p.height,
                    omsi_seat: *omsi_seat,
                }
            })
            .collect();
        let routes = build_routes(graph.points.len(), &links);
        let point_of = |i: i32| usize::try_from(i).ok().filter(|i| *i < graph.points.len());
        let stamper = data.stampers.last().map(|st| (point_of(st.path_point), Vec3::from(st.pos)));
        let sale = data.ticket_sales.last().map(|st| (point_of(st.path_point), Vec3::from(st.pos)));
        let money_point = data.money_points.last().map(|m| Vec3::from(m.pos));
        let money_var = data.money_points.last().map(|m| (Vec3::from(m.pos), m.var));
        let change_point = data.change_points.last().map(|m| Vec3::from(m.pos));
        Some(Cabin {
            data,
            graph,
            links,
            link_pack,
            step_packs,
            entries,
            exits,
            desk,
            seats,
            parts: cabin_parts,
            link_room,
            routes,
            stamper,
            sale,
            money_point,
            money_var,
            change_point,
        })
    }

    /// Every point of the path network (Omsi.exe's list +0xc of the paths).
    fn all_points(&self) -> Vec<Option<usize>> {
        (0..self.graph.points.len()).map(Some).collect()
    }











}

/// Whether the straight way from `a` to `b` goes over a carriageway: across the centre
/// line of a street lane (walking along the kerb on the carriageway's edge does not).
fn crosses_street(net: &Network, a: DVec2, b: DVec2) -> bool {
    let mut cells: Vec<(i32, i32)> = Vec::new();
    for p in [a, b, (a + b) * 0.5] {
        let c = Network::grid_cell(p.extend(0.0));
        if !cells.contains(&c) {
            cells.push(c);
        }
    }
    let mut seen: Vec<usize> = Vec::new();
    for c in cells {
        for &i in net.grid.get(&c).map(|v| v.as_slice()).unwrap_or(&[]) {
            if seen.contains(&i) {
                continue;
            }
            seen.push(i);
            let l = &net.lanes[i];
            if l.kind != LaneKind::Street {
                continue;
            }
            if l
                .points
                .windows(2)
                .any(|w| segments_cross(a, b, w[0].truncate(), w[1].truncate()))
            {
                return true;
            }
        }
    }
    false
}


/// Whether the segments `a`-`b` and `c`-`d` cross.
fn segments_cross(a: DVec2, b: DVec2, c: DVec2, d: DVec2) -> bool {
    let side = |p: DVec2, q: DVec2, r: DVec2| (q - p).perp_dot(r - p);
    let (d1, d2) = (side(c, d, a), side(c, d, b));
    let (d3, d4) = (side(a, b, c), side(a, b, d));
    d1 * d2 < 0.0 && d3 * d4 < 0.0
}

/// A bus as the passengers see it this frame.
#[derive(Clone)]
struct BusNow {
    id: BusId,
    cabin: Arc<Cabin>,
    pos: DVec3,
    rot: Mat4,
    heading: f64,
    /// m/s, forwards.
    speed: f64,
    entry_open: Vec<bool>,
    exit_open: Vec<bool>,
    /// The doors a walker may use (another player's bus: its doors as they are, while
    /// `entry_open` stays shut for the passengers here); None: as `entry_open`/`exit_open`.
    walk_open: Option<(Vec<bool>, Vec<bool>)>,
    interior: f32,
    /// The saloon's air and the light outside, for what boarding passengers say.
    air: CabinAir,
    /// Half extents across / along and the centre of its bounding box (bus frame).
    half: DVec2,
    centre: DVec2,
    /// Acceleration of the floor (bus frame: x to the right, y forwards; m/s²).
    accel: DVec2,
    /// The sections behind the front one (the cabin's parts after the first).
    trailers: Vec<PartFrame>,
    /// The terminus it shows, by name (Omsi.exe's bus +0x7bc). None: "$allexit$" - the
    /// scripts' `target_index_int` names a hof terminus added with `[addterminus_allexit]`
    /// ("Nicht einsteigen", a works trip) - or none; no timetable target has it.
    terminus: Option<String>,
}

/// What passengers feel stepping into a bus (OMSI reads the same fields: the vehicle's
/// `Cabinair_Temp` and `Cabinair_relHum`, the weather's temperature and the daylight).
#[derive(Debug, Clone, Copy, Default)]
struct CabinAir {
    /// °C, when the bus keeps its cabin air (every bus does: its script or the engine).
    temp: Option<f32>,
    /// Relative humidity, a fraction.
    rel_hum: f32,
    /// The temperature outside (°C).
    outside: f32,
    /// `Envir_Brightness`: the daylight, 0 dark .. 1.
    brightness: f32,
}

impl CabinAir {
    fn of(v: &VehicleInstance) -> CabinAir {
        CabinAir {
            temp: v.var("Cabinair_Temp").filter(|t| t.is_finite()),
            rel_hum: v.var("Cabinair_relHum").filter(|h| h.is_finite()).unwrap_or(0.0),
            outside: v.host.temperature,
            brightness: v.var("Envir_Brightness").unwrap_or(1.0),
        }
    }
}

/// Where a rear section of a bus is this frame, with its place in the cabin.
#[derive(Debug, Clone, Copy)]
struct PartFrame {
    pos: DVec3,
    rot: Mat4,
    heading: f64,
    offset: Vec3,
    joint_y: f32,
    /// Half extents across / along and the centre of its bounding box (own frame).
    half: DVec2,
    centre: DVec2,
}

/// The rear sections of `v` that are parts of `cabin`, as they stand now.
fn part_frames(v: &VehicleInstance, cabin: &Cabin) -> Vec<PartFrame> {
    cabin
        .parts
        .iter()
        .skip(1)
        .zip(&v.trailers)
        .map(|(cp, t)| {
            let bb =
                t.ty.def
                    .bounding_box
                    .unwrap_or([2.5, 7.0, 3.0, 0.0, 0.0, 1.5]);
            PartFrame {
                pos: t.position,
                rot: t.body_rotation(),
                heading: t.heading,
                offset: cp.offset,
                joint_y: cp.joint_y,
                half: DVec2::new(bb[0] as f64 * 0.5, bb[1] as f64 * 0.5),
                centre: DVec2::new(bb[3] as f64, bb[4] as f64),
            }
        })
        .collect()
}

/// How far into the frame of the section behind joint `t` a cabin point `y` lies: 0 in
/// front of the joint's blend, 1 behind it.
fn behind(t: &PartFrame, y: f32) -> f32 {
    ((JOINT_BLEND - (y - t.joint_y)) / (2.0 * JOINT_BLEND)).clamp(0.0, 1.0)
}

/// Where a point of a cabin (unfolded frame) is in the world: the front section carries
/// what lies ahead of the first joint, a rear section what lies behind its joint, and near
/// a joint the two are blended, so that somebody walking through the bellows moves on
/// smoothly however far the bus is bent.
fn train_point(pos: DVec3, rot: &Mat4, trailers: &[PartFrame], local: Vec3) -> DVec3 {
    let mut here = pos + rot.transform_point3(local).as_dvec3();
    for t in trailers {
        let w = behind(t, local.y);
        if w <= 0.0 {
            break;
        }
        let there = t.pos + t.rot.transform_point3(local - t.offset).as_dvec3();
        here = here.lerp(there, w as f64);
        if w < 1.0 {
            break;
        }
    }
    here
}

/// The heading of the floor at a point of a cabin (see [`train_point`]).
fn train_heading(heading: f64, trailers: &[PartFrame], local: Vec3) -> f64 {
    let mut here = heading;
    for t in trailers {
        let w = behind(t, local.y);
        if w <= 0.0 {
            break;
        }
        here += crowd::angle_diff(here, t.heading) * w as f64;
        if w < 1.0 {
            break;
        }
    }
    here
}

impl BusNow {
    fn world(&self, local: Vec3) -> DVec3 {
        train_point(self.pos, &self.rot, &self.trailers, local)
    }
    /// A world point in the cabin's frame (the inverse of `world`): the front section's,
    /// or a rear section's for a point behind its joint.
    fn to_local(&self, w: DVec3) -> Vec3 {
        let mut l = self.rot.inverse().transform_point3((w - self.pos).as_vec3());
        for t in &self.trailers {
            if l.y > t.joint_y {
                break;
            }
            l = t.rot.inverse().transform_point3((w - t.pos).as_vec3()) + t.offset;
        }
        l
    }
    /// The tilt (pitch and bank, in the world's axes, no heading) of the section a point of
    /// the cabin is in.
    fn tilt_at(&self, local: Vec3) -> Mat4 {
        let mut rot = self.rot;
        let mut heading = self.heading;
        for t in &self.trailers {
            if behind(t, local.y) < 0.5 {
                break;
            }
            rot = t.rot;
            heading = t.heading;
        }
        rot * Mat4::from_rotation_z(heading.to_radians() as f32)
    }
    /// The heading of the section a point of the cabin is in.
    fn heading_at(&self, local: Vec3) -> f64 {
        train_heading(self.heading, &self.trailers, local)
    }
    fn fwd(&self) -> DVec2 {
        let h = self.heading.to_radians();
        DVec2::new(h.sin(), h.cos())
    }
    /// The bodies people on the ground walk round: the bus and its rear sections.
    fn blocks(&self) -> Vec<Block> {
        let block = |pos: DVec3, heading: f64, half: DVec2, centre: DVec2| {
            let h = heading.to_radians();
            let (fwd, right) = (DVec2::new(h.sin(), h.cos()), DVec2::new(h.cos(), -h.sin()));
            Block {
                center: pos.truncate() + right * centre.x + fwd * centre.y,
                half,
                heading: h,
                vel: fwd * self.speed,
            }
        };
        let mut out = vec![block(self.pos, self.heading, self.half, self.centre)];
        out.extend(
            self.trailers
                .iter()
                .map(|t| block(t.pos, t.heading, t.half, t.centre)),
        );
        out
    }
}

/// One piece of a walk along a pavement path: lane `lane` from distance `a` to `b`.
#[derive(Debug, Clone, Copy)]
struct Leg {
    lane: usize,
    a: f32,
    b: f32,
}

impl Leg {
    fn len(&self) -> f32 {
        (self.b - self.a).abs()
    }
    fn dist(&self, p: f32) -> f32 {
        if self.b >= self.a {
            self.a + p
        } else {
            self.a - p
        }
    }
    /// Point and walking heading `p` metres into the leg.
    fn at(&self, net: &Network, p: f32) -> (DVec3, f64) {
        let (q, h) = net.lanes[self.lane].at(self.dist(p.clamp(0.0, self.len())));
        (
            q,
            if self.b >= self.a {
                h as f64
            } else {
                h as f64 + 180.0
            },
        )
    }
    /// How far into the leg the point nearest `pos` lies, looking around `hint`.
    fn project(&self, net: &Network, pos: DVec3, hint: f32) -> f32 {
        let (lo, hi) = ((hint - 1.5).max(0.0), (hint + 3.0).min(self.len()));
        let mut best = (hint, f64::MAX);
        let mut p = lo;
        while p <= hi + 1e-3 {
            let d = (self.at(net, p).0 - pos).truncate().length_squared();
            if d < best.1 {
                best = (p, d);
            }
            p += 0.2;
        }
        best.0
    }
    /// Whether the leg starts at an end of its lane (at a kerb or a junction).
    fn from_end(&self, net: &Network) -> bool {
        self.a < 0.05 || self.a > net.lanes[self.lane].length() - 0.05
    }
}

/// A walk along the pavement network.
#[derive(Debug, Clone)]
struct PedWalk {
    legs: Vec<Leg>,
    leg: usize,
    /// Metres walked into the current leg.
    s: f32,
    /// A stroll: goes on at random when the legs run out.
    roam: bool,
    /// Keep-right offset (m).
    side: f32,
    /// Seconds spent waiting at the kerb before the current leg.
    held: f32,
}

impl PedWalk {
    fn new(legs: Vec<Leg>, roam: bool, side: f32) -> PedWalk {
        PedWalk {
            legs,
            leg: 0,
            s: 0.0,
            roam,
            side,
            held: 0.0,
        }
    }
}

/// The pavement paths as a walking network: path ends closer than a metre are one
/// junction, whatever their heading (the road network joins lane ends only when they
/// continue in the same direction, which leaves every pavement corner open).
struct PedNet {
    /// Per pavement lane: its start and end junction.
    ends: HashMap<usize, (usize, usize)>,
    /// Per junction: (lane, walked forwards) leaving it.
    out: Vec<Vec<(usize, bool)>>,
    /// Where each pavement lane crosses a carriageway (lazily), and the carriageway lanes
    /// it crosses.
    crossings: HashMap<usize, Vec<DVec2>>,
    crossed: HashMap<usize, Vec<usize>>,
    /// Pavement lanes by 50 m cell.
    grid: HashMap<(i32, i32), Vec<usize>>,
    /// The junctions and a 1.5 m grid of them, for joining the paths of tiles loaded later.
    nodes: Vec<DVec3>,
    cells: HashMap<(i64, i64), Vec<usize>>,
    /// How many lanes of the traffic network are in (the network only grows: tiles bring
    /// their lanes and the indices stay).
    built: usize,
}

impl PedNet {
    fn build(net: &Network) -> PedNet {
        let mut p = PedNet {
            ends: HashMap::new(),
            out: Vec::new(),
            crossings: HashMap::new(),
            crossed: HashMap::new(),
            grid: HashMap::new(),
            nodes: Vec::new(),
            cells: HashMap::new(),
            built: 0,
        };
        p.extend(net);
        log::info!(
            "pavement network: {} paths, {} junctions",
            p.ends.len(),
            p.nodes.len()
        );
        p
    }

    /// Take in the lanes the network gained since the last call (tiles streamed in).
    fn extend(&mut self, net: &Network) -> usize {
        let from = self.built.min(net.lanes.len());
        let before = self.ends.len();
        for i in from..net.lanes.len() {
            let l = &net.lanes[i];
            if l.kind == LaneKind::Street && from > 0 {
                // a new carriageway may cross pavement paths that are in already
                self.crossings.clear();
            }
            if l.kind != LaneKind::Sidewalk || l.points.len() < 2 || l.length() < 0.3 {
                continue;
            }
            let a = self.node_of(l.start());
            let b = self.node_of(l.end());
            if a == b && l.length() < 3.0 {
                continue;
            }
            self.ends.insert(i, (a, b));
            self.out[a].push((i, true));
            self.out[b].push((i, false));
            let mut seen: Vec<(i32, i32)> = Vec::new();
            for p in &l.points {
                let c = ((p.x / 50.0).floor() as i32, (p.y / 50.0).floor() as i32);
                if !seen.contains(&c) {
                    seen.push(c);
                    self.grid.entry(c).or_default().push(i);
                }
            }
        }
        self.built = net.lanes.len();
        self.ends.len() - before
    }

    /// The junction at `p`, a new one when there is none within a metre.
    fn node_of(&mut self, p: DVec3) -> usize {
        let (cx, cy) = ((p.x / 1.5).floor() as i64, (p.y / 1.5).floor() as i64);
        for dx in -1..=1 {
            for dy in -1..=1 {
                if let Some(list) = self.cells.get(&(cx + dx, cy + dy)) {
                    for &n in list {
                        if (self.nodes[n] - p).truncate().length() < 1.2
                            && (self.nodes[n].z - p.z).abs() < 2.5
                        {
                            return n;
                        }
                    }
                }
            }
        }
        self.nodes.push(p);
        self.out.push(Vec::new());
        self.cells
            .entry((cx, cy))
            .or_default()
            .push(self.nodes.len() - 1);
        self.nodes.len() - 1
    }

    /// The pavement lane nearest `p` within `reach` that can be reached without going over
    /// a carriageway: (lane, distance along it, distance to it). The plain nearest one was
    /// often the pavement across the road - a passenger off a bus then walked straight
    /// over the carriageway through the traffic to it, or joined a crossing in the middle.
    fn nearest(&self, net: &Network, p: DVec3, reach: f64) -> Option<(usize, f32, f64)> {
        let (cx, cy) = ((p.x / 50.0).floor() as i32, (p.y / 50.0).floor() as i32);
        let mut cands: Vec<(usize, f32, f64)> = Vec::new();
        let mut seen = HashSet::new();
        for dx in -1..=1 {
            for dy in -1..=1 {
                for &i in self
                    .grid
                    .get(&(cx + dx, cy + dy))
                    .map(|v| v.as_slice())
                    .unwrap_or(&[])
                {
                    if !seen.insert(i) {
                        continue;
                    }
                    if let Some((s, d)) = net.lanes[i].nearest_point(p) {
                        if d < reach {
                            cands.push((i, s, d));
                        }
                    }
                }
            }
        }
        cands.sort_by(|a, b| a.2.total_cmp(&b.2));
        let first = cands.first().copied();
        cands
            .into_iter()
            .find(|&(i, s, _)| {
                let (q, _) = net.lanes[i].at(s);
                !crosses_street(net, p.truncate(), q.truncate())
                    && !self.crossings.get(&i).map(|x| !x.is_empty()).unwrap_or(false)
            })
            // on an island between carriageways: the nearest after all
            .or(first)
    }

    /// The junction a leg ends at, when it ends at one.
    fn end_node(&self, net: &Network, leg: &Leg) -> Option<usize> {
        let (a, b) = *self.ends.get(&leg.lane)?;
        let len = net.lanes[leg.lane].length();
        if leg.b < 0.05 {
            Some(a)
        } else if leg.b > len - 0.05 {
            Some(b)
        } else {
            None
        }
    }

    /// A leg leaving junction `node`, not back along `came` (unless it is a dead end).
    fn next_leg(&self, net: &Network, node: usize, came: usize, pick: u64) -> Option<Leg> {
        let back = self.ends.get(&came).copied();
        let twin = |l: usize| -> bool {
            l == came
                || matches!((self.ends.get(&l), back), (Some(&(a, b)), Some((c, d))) if a == d && b == c && (net.lanes[l].length() - net.lanes[came].length()).abs() < 1.0)
        };
        let list: Vec<(usize, bool)> = self
            .out
            .get(node)?
            .iter()
            .copied()
            .filter(|(l, _)| !twin(*l))
            .collect();
        // the way on rather than back: a path leaving the junction within 110° of the way
        // the walker came (there usually is one - a pavement goes on past a side street),
        // else any. Picked from all, a stroller would turn round at every corner and walk
        // back the way they came, which looked like a change of mind for no reason.
        let heading_in = {
            let l = &net.lanes[came];
            let (a, _) = back.unwrap_or((usize::MAX, usize::MAX));
            // arriving at `node` along `came`: forwards if its end is the node
            if a == node {
                wrap_heading(l.start_heading() as f64 + 180.0)
            } else {
                l.end_heading() as f64
            }
        };
        let leaving = |&(l, fwd): &(usize, bool)| -> f64 {
            let lane = &net.lanes[l];
            if fwd {
                lane.start_heading() as f64
            } else {
                wrap_heading(lane.end_heading() as f64 + 180.0)
            }
        };
        let onward: Vec<(usize, bool)> = list
            .iter()
            .copied()
            .filter(|o| angle_between(heading_in, leaving(o)) <= 110.0)
            .collect();
        let list = if onward.is_empty() { list } else { onward };
        let (lane, fwd) = if list.is_empty() {
            // a dead end: turn round
            let (a, _) = back?;
            (came, a == node)
        } else {
            list[(pick as usize) % list.len()]
        };
        let len = net.lanes[lane].length();
        Some(if fwd {
            Leg {
                lane,
                a: 0.0,
                b: len,
            }
        } else {
            Leg {
                lane,
                a: len,
                b: 0.0,
            }
        })
    }


    /// Where pavement lane `lane` crosses a carriageway.
    fn crossings(&mut self, net: &Network, lane: usize) -> &[DVec2] {
        if !self.crossings.contains_key(&lane) {
            let l = &net.lanes[lane];
            let mut cand: Vec<usize> = Vec::new();
            for p in &l.points {
                if let Some(list) = net
                    .grid
                    .get(&((p.x / 50.0).floor() as i32, (p.y / 50.0).floor() as i32))
                {
                    for &i in list {
                        if net.lanes[i].kind == LaneKind::Street && !cand.contains(&i) {
                            cand.push(i);
                        }
                    }
                }
            }
            let mut out = Vec::new();
            let mut crossed = Vec::new();
            for i in cand {
                let o = &net.lanes[i];
                for w in l.points.windows(2) {
                    for v in o.points.windows(2) {
                        if (w[0].z - v[0].z).abs() > 3.0 {
                            continue;
                        }
                        if let Some(x) = seg_cross(
                            w[0].truncate(),
                            w[1].truncate(),
                            v[0].truncate(),
                            v[1].truncate(),
                        ) {
                            if !crossed.contains(&i) {
                                crossed.push(i);
                            }
                            if !out.iter().any(|q: &DVec2| (*q - x).length() < 1.5) {
                                out.push(x);
                            }
                        }
                    }
                }
            }
            self.crossings.insert(lane, out);
            self.crossed.insert(lane, crossed);
        }
        &self.crossings[&lane]
    }

    /// The carriageway lanes a pavement lane crosses.
    fn crossed_lanes(&mut self, net: &Network, lane: usize) -> &[usize] {
        self.crossings(net, lane);
        &self.crossed[&lane]
    }
}

/// Seconds a pedestrian starting across `path` now has before a vehicle may drive over it:
/// until the first light of a carriageway lane it crosses (or of a lane leading into one)
/// turns green once the pedestrian green is over. Lanes that have green now, or get it
/// while the pedestrians still have theirs, are turning traffic that gives way. Without
/// such a light, the pedestrian green `green_left` and two seconds.
fn pedestrian_window(
    ped: Option<&mut PedNet>,
    net: &Network,
    traffic: &Traffic,
    path: usize,
    green_left: f32,
) -> f32 {
    let mut window = f32::MAX;
    if let Some(ped) = ped {
        for &s in ped.crossed_lanes(net, path) {
            let feeding = net.prev.get(s).map(|p| p.as_slice()).unwrap_or(&[]);
            for &l in std::iter::once(&s).chain(feeding) {
                let Some((c, li)) = net.lanes[l].traffic_light else {
                    continue;
                };
                if let Some(g) = traffic
                    .light_until_go(c, li)
                    .filter(|g| *g > 0.0 && *g >= green_left)
                {
                    window = window.min(g);
                }
            }
        }
    }
    if window == f32::MAX {
        green_left + 2.0
    } else {
        window
    }
}

fn seg_cross(a: DVec2, b: DVec2, c: DVec2, d: DVec2) -> Option<DVec2> {
    let r = b - a;
    let s = d - c;
    let den = r.perp_dot(s);
    if den.abs() < 1e-9 {
        return None;
    }
    let t = (c - a).perp_dot(s) / den;
    let u = (c - a).perp_dot(r) / den;
    (t >= 0.0 && t <= 1.0 && u >= 0.0 && u <= 1.0).then(|| a + r * t)
}

#[derive(Debug, Clone)]
enum State {
    /// A pedestrian strolling the pavements (Omsi.exe's task 8, `WalkStreet`).
    Strolling(PedWalk),
    /// Moved by somebody else: an avatar, or one of a LAN host's people.
    Idle,
    /// Task 8 without a path (+0x2f0 = -1): somebody who got off where no pavement is
    /// stands where they are until the player is gone.
    Standing,
    /// A passenger (see `humans_pax`).
    Pax(Box<Pax>),
}

impl State {
    fn name(&self) -> &'static str {
        match self {
            State::Strolling(_) => "WalkStreet",
            State::Idle => "Idle",
            State::Standing => "WalkStreet",
            State::Pax(p) => p.task.name(),
        }
    }
    fn bus(&self) -> Option<BusId> {
        match self {
            State::Pax(p) => p.inside.or(p.bus),
            _ => None,
        }
    }
}

/// Where a person is: on the ground, or inside a bus at a point of its frame.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Place {
    Ground,
    Bus(BusId, Vec3),
}



pub struct Person {
    id: u32,
    ty: Arc<HumanType>,
    /// Clothing variant (`HumanType::variant_texture`).
    variant: usize,
    meshes: Vec<(MeshId, usize)>,
    position: DVec3,
    heading: f64,
    /// Heading in the bus frame while inside one.
    lheading: f64,
    place: Place,
    /// Velocity in the plane the person walks in (ground or bus floor).
    vel: DVec2,
    pace: f64,
    activity: Activity,
    /// The animation: Omsi.exe's walk phase and joint angles (sub_626ae8).
    anim: OmsiAnim,
    state: State,
    /// Seconds in the current state.
    t_state: f32,
    /// Skinned positions and normals, per mesh.
    skins: Vec<(Vec<Vec3>, Vec<Vec3>)>,
    /// The bones the skins were made with, and whether this frame's pose changed them
    /// (somebody standing still keeps the mesh of the frame before: skinning and uploading
    /// thirty waiting people every frame took 2 ms of the frame at a bus station).
    skin_bones: Option<[glam::Affine3A; omsi_sim::human::SLOTS]>,
    pose_changed: bool,
    /// Interior light of the bus the person is in (0 outside).
    interior: f32,
    /// The interior light as drawn: it follows `interior` over a moment (stepping through
    /// the door, people lit up and went dark again from one frame to the next).
    lit: f32,
    /// The tilt of the floor the person stands on (a bus pitching under the brakes and
    /// leaning in a bend), without its heading: riders are drawn with it. Upright on the
    /// ground; drawn upright in a tilted bus, their feet sank through the floor on one side.
    tilt: Mat4,
    /// Age in years: the `.hum`'s `[age]`, else 40 as in OMSI. The
    /// ticket pack's tickets have age ranges (the reduced fare is for 6..13).
    age: f32,
    /// Seconds without getting nearer the goal while wanting to move; seconds left
    /// passing through others.
    stuck: f32,
    ghost: f32,
    /// Seconds a standing vehicle has stood in the way (see the crowd step).
    car_wait: f32,
    /// Seconds left going round something in the way off the pavement's line (a lamp post
    /// on the path): the corridor does not pull them back into it meanwhile.
    detour: f32,
    /// Which way round (+1 anticlockwise, -1 clockwise) while `detour` lasts: round a corner
    /// the sides' own choices flipped each other and people shuffled at a post.
    detour_side: f64,
    /// Why the person is standing, for `OMSI_DEBUG_PAX`.
    why: &'static str,
    /// Whether this person has ever been posed (an unposed model is the file's T-pose).
    skinned: bool,
    /// Frames since the last pose and where the person stood then (the
    /// feet of a mesh posed a frame ago stay on the floor when it is drawn there).
    since_posed: u32,
    posed_at: (DVec3, f64),
    /// Ankles of the last pose (model frame), for `OMSI_TRACE_PAX`.
    ankles: [Vec3; 2],
    /// A scripted test person (`OMSI_PAX_GALLERY`).
    puppet: Option<Puppet>,
    /// LAN play: one of the host's people, drawn where the host says (`mirror_set`).
    remote: bool,
}

impl Person {
    pub fn state_name(&self) -> String {
        format!("#{} {} ({})", self.id, self.state.name(), self.why)
    }
    pub fn position(&self) -> DVec3 {
        self.position
    }
    fn inside(&self, bus: BusId) -> bool {
        matches!(self.place, Place::Bus(b, _) if b == bus)
    }
    fn label(&self) -> String {
        format!(
            "#{} {}",
            self.id,
            self.ty
                .def
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        )
    }
}

/// What a person wants this frame.
struct Want {
    vel: DVec2,
    /// Heading to turn to when standing (world on the ground, bus frame inside).
    face: Option<f64>,
    give: f64,
    corridor: Option<(DVec2, DVec2, f64)>,
    /// What they do when not walking.
    idle: Activity,
}

impl Want {
    fn stand(face: Option<f64>, idle: Activity) -> Want {
        Want {
            vel: DVec2::ZERO,
            face,
            give: 0.35,
            corridor: None,
            idle,
        }
    }
}

/// Velocity towards `to`, easing into the stop over the last metre.
/// Somebody off the pavement's path by more than this beyond its corridor (just off a bus,
/// at its door) walks onto it at their own pace before the corridor holds them; within it
/// (a nudge of the crowd) the corridor takes them back.
const OFF_PATH: f64 = 0.1;

/// The corridor a walker at `pos` keeps to: none while they are still well off it. Eased
/// into it at up to 0.6 m/s (twice a step) on top of walking there, the people getting off
/// a bus slid sideways from its door to the pavement at twice their pace (#1033, #1079).
fn path_corridor(pos: DVec2, corridor: Option<(DVec2, DVec2, f64)>) -> Option<(DVec2, DVec2, f64)> {
    corridor.filter(|&(a, b, dev)| (crowd::clamp_to_corridor(pos, a, b, dev) - pos).length() <= OFF_PATH)
}

fn arrive(from: DVec2, to: DVec2, pace: f64) -> DVec2 {
    let d = to - from;
    let dist = d.length();
    if dist < 0.1 {
        return DVec2::ZERO;
    }
    let speed = (pace * dist.min(1.0)).max(if dist > 0.3 { 0.25 } else { 0.0 });
    d / dist * speed
}

/// Seconds after one passenger's greeting or complaint before anybody says another.
const CHAT_PAUSE: f64 = 12.0;

pub struct Humans {
    types: Vec<Arc<HumanType>>,
    pub people: Vec<Person>,
    rng: u64,
    next_id: u32,
    /// Seconds since the start.
    time: f64,
    wall_cells: HashMap<(i32, i32, i32), Vec<(Block, f64, f64)>>,
    wall_key: (usize, usize, usize, f64),
    /// Passenger cabins by vehicle files (the front vehicle and its coupled parts).
    cabins: HashMap<Vec<PathBuf>, Option<Arc<Cabin>>>,
    player_cabin: Option<Arc<Cabin>>,
    /// Which places of each bus are taken.
    seats: HashMap<BusId, Vec<bool>>,
    /// The bus stops as Omsi.exe keeps them for the people (see `humans_pax`).
    stops: HashMap<i64, PaxStop>,
    /// Kilometres each bus has driven (the odometer the riders read, +0x430).
    odometer: HashMap<BusId, f64>,
    /// The `PAX_Entry<n>_Req` / `PAX_Exit<n>_Req` of each bus this frame.
    pax_req: HashMap<BusId, (Vec<bool>, Vec<bool>)>,
    /// Who is at the player's cash desk (+0x7a8), how often the driver has been asked
    /// again (0x859bc4) and the most of that in this sale (0x859df4).
    desk_busy: Option<u32>,
    pardons: u8,
    pardon_max: u8,
    /// The first populate put people at the stops; later stops fill on foot when in sight.
    started: bool,
    ped: Option<PedNet>,
    hidden: Vec<usize>,
    /// GPU side of the human types, shared by everyone of a type: textures by file and the
    /// materials of every (type, mesh) - each person used to upload its own copies - and
    /// the meshes and instances of the people who have gone, taken over by the next person
    /// of the same type (the skinned vertices are rewritten anyway). Without that every
    /// passenger who ever appeared kept a mesh, its textures and materials on the GPU.
    gpu_textures: HashMap<PathBuf, Option<omsi_render::TextureId>>,
    /// Per (type, clothing variant, mesh): its materials, and the meshes and instances of
    /// people who have gone, kept for the next person dressed alike.
    gpu_materials: HashMap<(usize, usize, usize), Vec<MaterialId>>,
    spare: HashMap<(usize, usize, usize), Vec<(MeshId, usize)>>,
    /// Stop the player's bus is serving (standing at it).
    served_stop: Option<i64>,
    /// Timetable buses at a stop: id → (stop, time the visit began).
    ai_visits: HashMap<u64, (i64, f64)>,
    /// When each bus last had a door open (the passengers' clock).
    last_door_open: HashMap<BusId, f64>,
    /// Timetable buses to keep at their stop for a few seconds more (for the traffic): the
    /// bus, the stop it must be serving for it (none: any), the seconds.
    holds: Vec<(u64, Option<i64>, f32)>,
    /// Door requests for the timetable buses' scripts: (bus, entries, exits).
    ai_requests: Vec<(u64, Vec<bool>, Vec<bool>)>,
    pub tickets: Option<Arc<omsi_content::tickets::TicketPack>>,
    /// Current ticket request at the player's cash desk: (ticket name, value).
    pub request: Option<(String, f32)>,
    /// Payment on the desk: (paid, ticket value), and the change still owed after the ticket.
    pub paid: Option<(f32, f32)>,
    pub change_due: Option<f32>,
    pub money: Option<crate::money::Money>,
    /// A rider pressed the stop button for the next stop (the app fires the vehicle trigger `int_haltewunsch`).
    pub stop_request: bool,
    /// Tickets sold at the cash desk this session and what they were worth.
    pub tickets_sold: u32,
    pub ticket_cash: f32,
    /// Passengers that reached the cash desk, and those the driver served there.
    pub boarded: u32,
    pub served: u32,
    /// OMSI's rating counters: people who stepped into the player's bus
    /// and of those who had nothing to complain about (comfort = content / stepped in);
    /// tickets asked for and the points for selling them, two for the right change, one
    /// for the wrong (ticket selling = points / 2 × asked).
    pub stepped_in: u32,
    pub content: u32,
    pub ticket_requests: u32,
    pub ticket_points: u32,
    /// `PAX_Entry<i>_Req`: somebody at the kerb wants in through entry `i`.
    pub entry_req: Vec<bool>,
    /// `PAX_Exit<i>_Req`: somebody inside wants out through exit `i`.
    pub exit_req: Vec<bool>,
    sync_frame: u32,
    /// Feet put down since the app last collected them (see [`Humans::take_footfalls`]).
    footfalls: Vec<ambience::Footfall>,
    /// `[trafficdensity_passenger]` factor for the current hour (set by the app).
    pub density: f32,
    /// The clock's time of day in seconds (set by the app): day tickets sell by it.
    pub time_of_day: f64,
    /// How late the player's bus is on its duty (s; set by the app): over five minutes,
    /// boarding passengers may say so.
    pub delay: f64,
    /// The game's folder (the ticket pack's voices are found from it).
    root: std::path::PathBuf,
    /// What passengers said since the app last collected it (see `take_voice_lines`).
    voice_lines: Vec<VoiceLine>,
    /// When each voice file was last said (seconds of `time`): OMSI keeps such a list
    /// and says a greeting or a complaint only when that
    /// very file has not been heard for 10 s - without it every boarding passenger said
    /// "Hallo" one after the other.
    voice_said: HashMap<std::path::PathBuf, f64>,
    /// What passengers may say (the `pax_voices` setting): 0 everything, 1 only the
    /// ticket they ask for, 2 nothing.
    pub voices: u8,
    /// When anybody last greeted or complained (seconds of `time`).
    last_chat: f64,
    /// Avatars (the player on foot, other players' walkers): key → person id, and what
    /// the game wants of each this frame.
    avatars: HashMap<u32, u32>,
    avatar_cmds: HashMap<u32, AvatarCmd>,
    /// Avatars not drawn (the first-person view), by person id.
    avatar_hidden: HashMap<u32, bool>,
    /// The buses of the last tick (for the avatars' seats and doors).
    last_buses: Vec<BusNow>,
    /// Only avatars: nobody else is put on the map (the passengers are off).
    pub avatar_only: bool,
    /// The player has got up and left the wheel: a standing bus with a door open is left
    /// by its riders as at a terminus (see `ALL_OUT_STOP`).
    pub driver_away: bool,
    /// Per bus stop, Omsi.exe's station targets (0x61cb18, `Schedule::stop_targets`): the
    /// stops the trips go on to, each with the termini of those trips. A person waiting there
    /// wants one of them and boards only a bus showing one of its termini; at a stop no trip
    /// goes on from, anybody takes the first bus (0x61c33c).
    pub stop_targets: Option<HashMap<i64, Vec<(String, HashSet<String>)>>>,
    /// The timetable's name of each stop object (`Schedule::stop_names`), the names the
    /// targets above are made of.
    pub stop_names: Option<HashMap<i64, String>>,
    /// Buses whose validator somebody used since the app last looked (`take_stamped`).
    stamped: Vec<BusId>,
    /// Pedestrians to keep strolling near the player (scaled by `density`).
    pub pedestrians: usize,
    /// Omsi.exe's people (`[AIMaxCountRandom]`'s second line, the `ai_max_humans` setting):
    /// it makes that many at the start (0x709274) and draws everybody waiting at a stop,
    /// walking the pavements or riding from them - never more; at most half of them walk
    /// the pavements (0x62463c). Here people are made as they are wanted, so they are
    /// counted against it instead.
    pub max_people: usize,
    stroll_timer: f32,
    /// Passengers pay the exact fare: no change is ever due.
    pub exact_fare: bool,
    /// How passengers board (`boarding` in the settings): `auto` - pay and take the
    /// ticket by themselves after a moment; `pay` - wait at the desk for the driver to
    /// sell it (and show a pass after `PAY_PATIENCE`); `walk` - no cash desk at all.
    pub boarding: String,
    /// The driver pressed the ticket key (`ticket_give`): sell the requested ticket.
    pub give_ticket: bool,
    /// The driver pressed `change_give`: all the change owed goes on the tray at once.
    pub give_change_all: bool,
    /// The key that sells a ticket, as the HUD names it.
    pub ticket_key: String,
    /// Where the player looks from (set by the app every frame).
    pub eye: Option<Eye>,
    /// Around where people are kept (the player's bus, else the camera).
    center: DVec3,
    /// A line for the HUD about something that just happened.
    message: Option<String>,
    /// Frames ticked, total and longest tick (ms).
    tick_stats: (u32, f64, f64),
    /// Where the time of this tick went (stage, ms since the one before), for the slow
    /// ticks OMSI_PROFILE reports.
    tick_stages: Vec<(&'static str, f64)>,
    /// Speed, heading and floor acceleration of each bus last frame (for the riders' balance).
    bus_motion: HashMap<BusId, (f64, f64, DVec2)>,
    /// `types` has been cut down to the map's `humans.txt`.
    map_humans_done: bool,
    /// Simulation time of the last `sync`.
    last_sync: f64,
    /// Frames synced, people posed and skinned, the time that took and the part of it spent
    /// uploading (ms), in total.
    pose_stats: (u32, usize, f64, f64),
    /// `OMSI_TRACE_PAX=<csv>`: every person near the eye, every frame (see `sync`).
    trace: Option<std::io::BufWriter<std::fs::File>>,
    /// `World::tiles_generation` the stops were last checked against.
    tiles_seen: u64,
    /// LAN play: this game draws the host's people instead of its own (`lan_world`).
    mirror: bool,
    /// LAN play: where the other players are (host): people are kept around them too.
    pub lan_centers: Vec<DVec3>,
    /// LAN play: the other players' buses this frame (`set_remote_buses`), for their riders
    /// to sit in. Nobody of ours boards them: their doors count as shut.
    remote_now: Vec<BusNow>,
    /// The vehicles the player placed and is not driving now (`placed_bus_id`): their
    /// riders stay in them when the player drives another.
    placed_now: Vec<BusNow>,
    /// LAN play (client): waiting people our bus could take, to ask the host for, and when
    /// each was last asked for.
    claims_out: Vec<u32>,
    claimed: HashMap<u32, f64>,
    /// The host's people waiting at a stop (client): (stop, waiting place).
    mirror_wait: HashMap<u32, (i64, usize)>,
    /// How the player's bus is driven, for its riders' complaints.
    comfort: RideComfort,
    /// LAN play (host): the waiting people handed over to another player's bus, by stop and
    /// that bus. They count among the people of the stop while the bus stands there, as
    /// the people who board a bus of ours keep their stop until it has left: without them
    /// the stop filled up again at once - one more person a frame once its 10..15 s were
    /// up - and the client's bus took them all, one stream of passengers that never ended
    /// (#842, #840, #830).
    handed: Vec<(i64, u64)>,
}

/// Resolve each map entry directly, including human packs with nested folders.
/// Keep duplicate entries as spawn weights, but load each definition only once.
fn map_human_types(root: &Path, list: &[String]) -> Vec<Arc<HumanType>> {
    // (keyed case-blind: OMSI paths are, and the lists spell one file several ways)
    let mut loaded: HashMap<String, Option<Arc<HumanType>>> = HashMap::new();
    let mut picked = Vec::new();
    for line in list {
        let rel = line.trim().replace('\\', "/");
        // Lists normally include Humans/, but also accept paths relative to that folder.
        let rel = if rel.to_ascii_lowercase().starts_with("humans/") {
            rel
        } else {
            format!("Humans/{rel}")
        };
        let path = omsi_cfg::resolve_path(root, &rel);
        let ty = loaded.entry(path.to_string_lossy().to_lowercase()).or_insert_with(|| {
            match HumanType::load(&path) {
                Ok(t) => Some(Arc::new(t)),
                Err(e) => {
                    log::warn!("map human {}: {e:#}", path.display());
                    None
                }
            }
        });
        if let Some(t) = ty {
            picked.push(t.clone());
        }
    }
    picked
}

impl Humans {
    /// LAN uses the room id as the shared source of randomness.  This keeps the
    /// initial pedestrian selection and their generated identities identical on
    /// the host and clients; subsequent movement remains simulation-local.
    pub fn set_lan_seed(&mut self, seed: u64) {
        self.rng = (seed ^ 0xA5A5_5A5A_1F2E_3D4C) as u64 | 1;
    }

    pub fn new(root: &Path) -> Humans {
        let mut types = Vec::new();
        // `Humans/<group>/*.hum` of every content root (an installed map or mod brings its
        // own people); a file of the same group and name higher up replaces the stock one
        let mut roots = omsi_cfg::content_dirs("Humans");
        if roots.is_empty() {
            roots.push(root.join("Humans"));
        }
        // (group, file name, path), sorted by group and name as the single folder used to be
        let mut found: Vec<(std::ffi::OsString, std::ffi::OsString, std::path::PathBuf)> =
            Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for r in &roots {
            for (group, is_dir) in omsi_cfg::vfs::list_dir(r).unwrap_or_default() {
                if !is_dir {
                    continue;
                }
                let d = r.join(&group);
                for (n, _) in omsi_cfg::vfs::list_dir(&d).unwrap_or_default() {
                    let lower = n.to_string_lossy().to_ascii_lowercase();
                    if !lower.ends_with(".hum") || lower.contains("driver") {
                        continue;
                    }
                    if seen.insert(format!(
                        "{}/{lower}",
                        group.to_string_lossy().to_ascii_lowercase()
                    )) {
                        found.push((group.clone(), n.clone(), d.join(&n)));
                    }
                }
            }
        }
        found.sort();
        let files: Vec<std::path::PathBuf> = found.into_iter().map(|(_, _, p)| p).collect();
        for f in files {
            match HumanType::load(&f) {
                Ok(t) => types.push(Arc::new(t)),
                Err(e) => log::warn!("human {}: {e:#}", f.display()),
            }
        }
        log::info!("humans: {} types", types.len());
        if omsi_cfg::env::var_os("OMSI_DEBUG_HUMANS").is_some() {
            for t in &types {
                let (mut lo, mut hi) = (f32::MAX, f32::MIN);
                for m in &t.meshes {
                    for v in &m.data.positions {
                        lo = lo.min(v.z);
                        hi = hi.max(v.z);
                    }
                }
                log::info!(
                    "  {} z {lo:.2}..{hi:.2}",
                    t.def.path.file_name().unwrap_or_default().to_string_lossy()
                );
            }
        }
        Humans {
            types,
            people: Vec::new(),
            rng: 0x1234_5678_9ABC_DEF1,
            next_id: 1,
            time: 0.0,
            wall_cells: HashMap::new(),
            wall_key: (0, 0, 0, 0.0),
            cabins: HashMap::new(),
            player_cabin: None,
            seats: HashMap::new(),
            stops: HashMap::new(),
            odometer: HashMap::new(),
            pax_req: HashMap::new(),
            desk_busy: None,
            pardons: 0,
            pardon_max: 0,
            started: false,
            ped: None,
            hidden: Vec::new(),
            gpu_textures: HashMap::new(),
            gpu_materials: HashMap::new(),
            spare: HashMap::new(),
            served_stop: None,
            ai_visits: HashMap::new(),
            last_door_open: HashMap::new(),
            holds: Vec::new(),
            ai_requests: Vec::new(),
            tickets: None,
            request: None,
            paid: None,
            change_due: None,
            money: None,
            stop_request: false,
            tickets_sold: 0,
            ticket_cash: 0.0,
            boarded: 0,
            served: 0,
            stepped_in: 0,
            content: 0,
            ticket_requests: 0,
            ticket_points: 0,
            entry_req: Vec::new(),
            exit_req: Vec::new(),
            sync_frame: 0,
            footfalls: Vec::new(),
            density: 1.0,
            time_of_day: 12.0 * 3600.0,
            delay: 0.0,
            root: root.to_path_buf(),
            voice_lines: Vec::new(),
            voice_said: HashMap::new(),
            voices: 0,
            last_chat: -1e9,
            avatars: HashMap::new(),
            avatar_cmds: HashMap::new(),
            avatar_hidden: HashMap::new(),
            last_buses: Vec::new(),
            avatar_only: false,
            driver_away: false,
            stop_targets: None,
            stop_names: None,
            stamped: Vec::new(),
            pedestrians: 14,
            max_people: crate::settings::Settings::load().ai_max_humans.max(1) as usize,
            stroll_timer: 0.0,
            exact_fare: true,
            boarding: "auto".into(),
            give_ticket: false,
            give_change_all: false,
            ticket_key: "T".into(),
            eye: None,
            center: DVec3::ZERO,
            message: None,
            tick_stats: (0, 0.0, 0.0),
            tick_stages: Vec::new(),
            bus_motion: HashMap::new(),
            map_humans_done: false,
            last_sync: 0.0,
            pose_stats: (0, 0, 0.0, 0.0),
            trace: omsi_cfg::env::var("OMSI_TRACE_PAX").ok().and_then(|f| std::fs::File::create(f).ok()).map(|f| {
                use std::io::Write;
                let mut w = std::io::BufWriter::new(f);
                let _ = writeln!(w, "t,id,state,ground,posed,x,y,z,heading,lx,ly,lz,rx,ry,rz,vx,vy");
                w
            }),
            tiles_seen: 0,
            mirror: false,
            lan_centers: Vec::new(),
            remote_now: Vec::new(),
            placed_now: Vec::new(),
            claims_out: Vec::new(),
            claimed: HashMap::new(),
            mirror_wait: HashMap::new(),
            comfort: RideComfort::default(),
            handed: Vec::new(),
        }
    }

    /// The map's tiles changed: a stop gone with its tile takes the people waiting there
    /// with it; a stop nobody waits at is set up again with what its tiles hold now (its
    /// waiting places come with the objects round it, sub_620c0c).
    fn tiles_changed(&mut self, world: &World) {
        let present: HashSet<i64> = world.bus_stops.lock().iter().map(|s| s.0).collect();
        let bound = |st: &State| -> Option<i64> {
            match st {
                State::Pax(p) if p.inside.is_none() => p.stop,
                _ => None,
            }
        };
        let used: HashSet<i64> = self.people.iter().filter_map(|p| bound(&p.state)).collect();
        let gone: Vec<i64> = self.stops.keys().copied().filter(|id| !present.contains(id)).collect();
        let mut removed = 0usize;
        for i in (0..self.people.len()).rev() {
            let p = &self.people[i];
            let lost_stop = bound(&p.state).is_some_and(|s| gone.contains(&s));
            let lost_ground = p.place == Place::Ground && p.puppet.is_none() && !world.has_ground(p.position.x, p.position.y);
            if lost_stop || lost_ground {
                self.release(i);
                let p = self.people.swap_remove(i);
                if debug_pax() {
                    log::info!("t={:.1} pax {} taken away with its tile ({})", self.time, p.label(), p.state.name());
                }
                self.retire(&p);
                removed += 1;
            }
        }
        for id in &gone {
            self.stops.remove(id);
        }
        let idle: Vec<i64> = self.stops.keys().copied().filter(|id| !used.contains(id)).collect();
        let rebuilt = idle.len();
        for id in idle {
            self.stops.remove(&id);
        }
        if debug_pax() || (omsi_cfg::env::var_os("OMSI_PROFILE").is_some() && (removed > 0 || !gone.is_empty())) {
            log::info!("people: tiles changed: {} stops gone, {rebuilt} set up again, {removed} people taken away", gone.len());
        }
    }

    /// The buses somebody stamped a ticket in since the last call (the app fires their
    /// `ev_Stamper` sound trigger): `None` the player's, else the AI car's id.
    pub fn take_stamped(&mut self) -> Vec<Option<u64>> {
        std::mem::take(&mut self.stamped)
            .into_iter()
            .map(|b| match b {
                BusId::Ai(id) => Some(id),
                _ => None,
            })
            .collect()
    }

    /// Lines passengers said since the last call (the app plays them where they stand).
    pub fn take_voice_lines(&mut self) -> Vec<VoiceLine> {
        std::mem::take(&mut self.voice_lines)
    }


    /// `limited`: said only when the same file has not been said for 10 s (greetings and
    /// complaints; the ticket asked for, "thanks" and the missing change always are).
    fn say_ex(&mut self, i: usize, name: &str, limited: bool) {
        // the player may have silenced them (settings), all but the ticket they ask for
        match self.voices {
            2 => return,
            1 if !name.starts_with("Ticket_") => return,
            _ => {}
        }
        // Greetings and complaints: one at a time for the whole bus. OMSI only keeps
        // the same file from being said twice within 10 s, and with a dozen people
        // boarding every other one said hello - the saloon never stopped talking, which
        // is not how the original sounds: a few words now and then.
        if limited && self.time - self.last_chat < CHAT_PAUSE && self.time >= self.last_chat {
            return;
        }
        // (without a `[voicepath]` the pack's own folder: Berlin_1 and Berlin_86 carry the
        // voices themselves and name no path; the later packs point at theirs)
        let Some(base) = self.tickets.as_ref().and_then(|t| match &t.voice_path {
            Some(vp) if !vp.trim().is_empty() => Some(omsi_cfg::resolve_path(&self.root, vp.trim())),
            _ => t.path.parent().map(|p| p.to_path_buf()),
        }) else {
            return;
        };
        let voice = self.people[i].ty.def.voice.trim().to_string();
        if voice.is_empty() {
            return;
        }
        let dir = omsi_cfg::resolve_path(&base, &voice);
        let path = omsi_cfg::resolve_path(&dir, &format!("{name}.wav"));
        if !omsi_cfg::vfs::is_file(&path) {
            return;
        }
        if limited {
            if let Some(&t) = self.voice_said.get(&path) {
                if self.time - t < 10.0 && self.time >= t {
                    return;
                }
            }
        }
        self.voice_said.insert(path.clone(), self.time);
        if limited {
            self.last_chat = self.time;
        }
        if debug_pax() {
            log::info!("t={:.1} pax {} says {name}", self.time, self.people[i].label());
        }
        self.voice_lines.push(VoiceLine { position: self.people[i].position + DVec3::new(0.0, 0.0, 1.6), path });
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

    /// Whether the player could see somebody standing at `p`.
    fn seen(&self, p: DVec3) -> bool {
        match self.eye {
            None => (p - self.center).length() < 150.0,
            Some(e) => {
                let d = p + DVec3::Z * 0.9 - e.pos;
                let dist = d.length();
                if dist > 230.0 {
                    return false;
                }
                dist < 3.0 || d.dot(e.fwd) / dist > e.cos_half
            }
        }
    }

    /// The people of OMSI's pool there are now (everybody but avatars and other players'
    /// people mirrored here).
    fn pool_used(&self) -> usize {
        self.people.iter().filter(|p| p.puppet.is_none() && !p.remote).count()
    }

    /// Room in the pool for one more person; when it is full somebody walking the street out
    /// of sight is taken for it, as Omsi.exe takes a task-8 person for a stop (0x61bd44).
    fn pool_room(&mut self) -> bool {
        if self.pool_used() < self.max_people {
            return true;
        }
        let free = (0..self.people.len()).find(|&i| {
            let p = &self.people[i];
            p.puppet.is_none() && !p.remote && matches!(p.state, State::Strolling(_) | State::Standing) && !self.seen(p.position)
        });
        match free {
            Some(i) => {
                self.release(i);
                let p = self.people.swap_remove(i);
                self.retire(&p);
                true
            }
            None => false,
        }
    }

    /// The cabin of a vehicle with the parts coupled behind it.
    fn cabin_for(&mut self, v: &VehicleInstance) -> Option<Arc<Cabin>> {
        let parts = train_parts(v);
        let key: Vec<PathBuf> = parts.iter().map(|p| p.0.path.clone()).collect();
        if let Some(c) = self.cabins.get(&key) {
            return c.clone();
        }
        let cabin = Cabin::load_train(&parts).map(Arc::new);
        if let Some(c) = cabin.as_ref().filter(|c| c.parts.len() > 1) {
            log::info!("passenger cabin of {}: {} sections joined ({} places, {} entries, {} exits, {} path points)", v.ty.def.path.file_name().unwrap_or_default().to_string_lossy(), c.parts.len(), c.seats.len(), c.entries.len(), c.exits.len(), c.graph.points.len());
        }
        self.cabins.insert(key, cabin.clone());
        cabin
    }

    /// The footsteps taken since the last call, for the environment sounds. They pile up
    /// only between two frames; a run without audio never looks at them, so the list is
    /// dropped once it grows past a crowd's worth of steps.
    /// `OMSI_TRACE_PAX` is writing a trace.
    pub fn tracing(&self) -> bool {
        self.trace.is_some()
    }

    pub fn take_footfalls(&mut self) -> Vec<ambience::Footfall> {
        if self.footfalls.len() > 256 {
            self.footfalls.clear();
        }
        std::mem::take(&mut self.footfalls)
    }

    /// Everybody: (walking, waiting at a stop, in a bus).
    pub fn counts(&self) -> (usize, usize, usize) {
        let (mut walking, mut waiting, mut aboard) = (0, 0, 0);
        for p in &self.people {
            match (&p.place, &p.state) {
                (Place::Bus(..), _) => aboard += 1,
                (Place::Ground, State::Pax(x)) if x.inside.is_none() => waiting += 1,
                (Place::Ground, _) => walking += 1,
            }
        }
        (walking, waiting, aboard)
    }

    /// People currently in the player's bus.
    pub fn riding(&self) -> usize {
        self.people
            .iter()
            .filter(|p| p.inside(BusId::Player))
            .count()
    }

    /// People walking the footpaths of the traffic network: (lane, distance along it). The
    /// traffic gives way to them at crossings and presses the pedestrian lights' buttons
    /// for them.
    /// Everybody on foot on the ground, for the traffic to stop for: position, velocity
    /// and whether they wait at a stop (a bus pulls up right beside those).
    pub fn on_foot(&self) -> Vec<(DVec3, DVec2, bool)> {
        self.people
            .iter()
            .filter(|p| p.place == Place::Ground)
            .map(|p| {
                let waiting = matches!(&p.state, State::Pax(x) if x.inside.is_none());
                (p.position, p.vel, waiting)
            })
            .collect()
    }

    pub fn strollers(&self) -> Vec<(usize, f32)> {
        self.people
            .iter()
            .filter_map(|p| match &p.state {
                State::Strolling(walk) => walk
                    .legs
                    .get(walk.leg)
                    .map(|leg| (leg.lane, leg.dist(walk.s))),
                _ => None,
            })
            .collect()
    }

    /// Seats of the player's bus from its `[passengercabin]`, and the engine's side of the
    /// ticket printer: `GivenTicket` is -1 until the driver hands a ticket over (the stock
    /// `Ticketprinter.osc` never sets it, OMSI starts it at -1 - left at 0 the first
    /// passenger took ticket 0 without the driver doing anything).
    pub fn set_cabin(&mut self, vehicle: &mut VehicleInstance) {
        vehicle.set_engine_var("GivenTicket", -1.0);
        match self.cabin_for(vehicle) {
            Some(c) => {
                log::info!("passenger cabin: {} places ({} seats), {} entries, {} exits, {} path points, desk {:?}", c.seats.len(), c.seats.iter().filter(|s| s.seated).count(), c.entries.len(), c.exits.len(), c.graph.points.len(), c.desk.map(|d| d.0));
                if debug_pax() {
                    for (i, e) in c.entries.iter().enumerate() {
                        log::info!(
                            "  entry {i}: inside {:?} wait {:?} sells {}",
                            e.inside,
                            e.wait,
                            e.sells
                        );
                    }
                    for (i, e) in c.exits.iter().enumerate() {
                        log::info!("  exit {i}: inside {:?} wait {:?}", e.inside, e.wait);
                    }
                    for (i, s) in c.seats.iter().enumerate() {
                        log::info!(
                            "  seat {i}: pos {:?} floor {:?} rot {:.0} seated {}",
                            s.pos,
                            s.floor,
                            s.rot,
                            s.seated
                        );
                    }
                }
                self.seats.insert(BusId::Player, vec![false; c.seats.len()]);
                self.entry_req = vec![false; c.entries.len().max(1)];
                self.exit_req = vec![false; c.exits.len().max(1)];
                self.player_cabin = Some(c);
            }
            None => log::info!("{}: no passenger cabin", vehicle.ty.def.path.display()),
        }
    }





    fn spawn(
        &mut self,
        world: &World,
        renderer: &Renderer,
        scene: &mut Scene,
        position: DVec3,
        heading: f64,
        state: State,
    ) -> Option<usize> {
        self.spawn_as(world, renderer, scene, position, heading, state, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_as(
        &mut self,
        world: &World,
        renderer: &Renderer,
        scene: &mut Scene,
        position: DVec3,
        heading: f64,
        state: State,
        kind: Option<usize>,
    ) -> Option<usize> {
        self.use_map_humans(world);
        if self.types.is_empty() {
            return None;
        }
        // on the surface they will walk on, not on the bare terrain under a pavement
        // (they stood in the asphalt and climbed out of it when they started walking)
        let mut position = position;
        if let Some(z) = world.walk_height_near(position.x, position.y, position.z) {
            if (z - position.z).abs() < 3.0 {
                position.z = z;
            }
        }
        // Not a twin of somebody standing near: two of the same figure in the same clothes
        // side by side at a stop was the first thing one noticed. A few tries for a figure
        // nobody near wears (then at least other clothes); with few figures installed some
        // repeat anyway.
        let near: Vec<(usize, usize)> = self
            .people
            .iter()
            .filter(|q| (q.position - position).truncate().length() < 30.0)
            .map(|q| (Arc::as_ptr(&q.ty) as usize, q.variant))
            .collect();
        let mut choice: Option<(usize, usize)> = None;
        for attempt in 0..10 {
            let pick = (self.rand() % self.types.len() as u64) as usize;
            let idx = kind.map(|k| k % self.types.len()).unwrap_or(pick);
            let t = &self.types[idx];
            let tk = Arc::as_ptr(t) as usize;
            // the default clothes or one of the `.cti` variants, alike likely
            let n_var = t.variants.len() as u64 + 1;
            let v0 = (self.rand() % n_var) as usize;
            // a clothing variant nobody near wears in this figure
            let var = (0..n_var as usize).map(|k| (v0 + k) % n_var as usize).find(|v| !near.contains(&(tk, *v)));
            let figure_free = !near.iter().any(|n| n.0 == tk);
            match var {
                Some(v) if figure_free || attempt >= 6 || kind.is_some() => {
                    choice = Some((idx, v));
                    break;
                }
                Some(v) if choice.is_none() => choice = Some((idx, v)),
                None if choice.is_none() && attempt == 9 => choice = Some((idx, v0)),
                _ => {}
            }
        }
        let (idx, variant) = choice.unwrap_or((0, 0));
        let ty = self.types[idx].clone();
        let tkey = Arc::as_ptr(&ty) as usize;
        let mut meshes = Vec::new();
        for (mi, hm) in ty.meshes.iter().enumerate() {
            let key = (tkey, variant, mi);
            // somebody of this type has gone: their mesh and instance
            if let Some((id, inst)) = self.spare.get_mut(&key).and_then(|v| v.pop()) {
                self.hidden.retain(|h| *h != inst);
                renderer.set_transform(scene, inst, position, Mat4::IDENTITY);
                renderer.set_params(scene, inst, &[], true, &[]);
                meshes.push((id, inst));
                continue;
            }
            if !self.gpu_materials.contains_key(&key) {
                let dirs = ty.texture_dirs(&world.root);
                let mut mats = Vec::new();
                for (k, m) in hm.materials.iter().enumerate() {
                    // the variant's texture from its own folder first, else the default
                    let (name, first) = ty.variant_texture(&m.texture, variant);
                    let mut look: Vec<&Path> = first.into_iter().collect();
                    look.extend(dirs.iter().map(|p| p.as_path()));
                    let found = omsi_texture::find_texture(name, &look)
                        .or_else(|| omsi_texture::find_texture(&m.texture, &look));
                    if found.is_none() && !m.texture.trim().is_empty() {
                        log::warn!(
                            "human {}: texture {} not found",
                            ty.def.path.display(),
                            m.texture
                        );
                    }
                    let tex = match found {
                        Some(path) => match self.gpu_textures.get(&path) {
                            Some(t) => *t,
                            None => {
                                let t = world
                                    .textures
                                    .get_gpu_fast(&path)
                                    .map(|(img, _)| renderer.add_texture_data(scene, &img));
                                world.textures.release(&path);
                                self.gpu_textures.insert(path, t);
                                t
                            }
                        },
                        None => None,
                    };
                    let alpha = match hm.alpha.get(k).copied().unwrap_or(0) {
                        1 => AlphaMode::Test,
                        2 => AlphaMode::Blend,
                        _ => AlphaMode::Opaque,
                    };
                    mats.push(renderer.add_material(scene, tex, alpha, [1.0; 4], false));
                }
                self.gpu_materials.insert(key, mats);
            }
            let mats = self.gpu_materials[&key].clone();
            let id = renderer.add_mesh(scene, &hm.data);
            let inst = renderer.add_instance(scene, id, position, Mat4::IDENTITY, mats);
            meshes.push((id, inst));
        }
        // walking pace 1.1 m/s +- 0.2, as Omsi.exe draws it for everybody (0x625758:
        // sub_7f08b0(0.2, 1.1)); `[walk_param]` holds the stride, not a speed
        let pace = 1.1 + (self.rand_f() * 2.0 - 1.0) * 0.2;
        let age = ty.def.age.map(|a| a as f32).unwrap_or(40.0);
        let id = self.next_id;
        self.next_id += 1;
        if debug_pax() {
            log::info!(
                "pax #{id} ({}) appears at ({:.1}, {:.1}, {:.1}): {}{}",
                ty.def
                    .path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy(),
                position.x,
                position.y,
                position.z,
                state.name(),
                if self.seen(position) { " IN SIGHT" } else { "" }
            );
        }
        self.people.push(Person {
            id,
            ty,
            variant,
            meshes,
            position,
            heading,
            lheading: 0.0,
            place: Place::Ground,
            vel: DVec2::ZERO,
            pace,
            activity: Activity::Stand,
            anim: OmsiAnim::default(),
            state,
            t_state: 0.0,
            skins: Vec::new(),
            skin_bones: None,
            pose_changed: false,
            interior: 0.0,
            lit: 0.0,
            tilt: Mat4::IDENTITY,

            age,
            stuck: 0.0,
            ghost: 0.0,
            car_wait: 0.0,
            detour: 0.0,
            detour_side: 0.0,
            why: "",
            skinned: false,
            since_posed: 0,
            posed_at: (position, heading),
            ankles: [Vec3::ZERO; 2],
            puppet: None,
            remote: false,
        });
        Some(self.people.len() - 1)
    }

    /// Someone has gone: hidden, and their meshes kept for the next person of the type.
    fn retire(&mut self, p: &Person) {
        let tkey = Arc::as_ptr(&p.ty) as usize;
        for (mi, m) in p.meshes.iter().enumerate() {
            self.hidden.push(m.1);
            self.spare.entry((tkey, p.variant, mi)).or_default().push(*m);
        }
    }


    /// A ticket of the pack for a passenger of `age`: those whose age
    /// range holds it, weighted by their probability - a day ticket's by the time of day
    /// as well (`day_ticket_factor`). `max_stations` plays no part in the choice.
    fn pick_ticket(&mut self, age: f32) -> Option<usize> {
        let r = self.rand_f() as f32;
        let day = day_ticket_factor(self.time_of_day);
        let t = self.tickets.as_ref()?;
        let weight = |tk: &omsi_content::tickets::Ticket| {
            if (tk.age_min as f32) > age || (tk.age_max as f32) < age {
                0.0
            } else if tk.day_ticket {
                tk.probability.max(0.0) * day
            } else {
                tk.probability.max(0.0)
            }
        };
        let total: f32 = t.tickets.iter().map(weight).sum();
        if total <= 0.0 {
            return None;
        }
        let mut x = r * total;
        for (i, tk) in t.tickets.iter().enumerate() {
            let w = weight(tk);
            if w > 0.0 && x < w {
                return Some(i);
            }
            x -= w;
        }
        None
    }



    fn free_seat(&mut self, bus: BusId, seat: usize) {
        if let Some(t) = self.seats.get_mut(&bus).and_then(|v| v.get_mut(seat)) {
            *t = false;
        }
    }



    /// Put people at the bus stops near `center`: at the start everywhere, later only at
    /// stops out of sight (the others fill with people walking up).
    pub fn populate(
        &mut self,
        world: &World,
        renderer: &Renderer,
        scene: &mut Scene,
        center: DVec3,
    ) {
        if self.avatar_only {
            return;
        }
        // whoever stands under the surface there (its tile's pavements and roads came
        // after them), or anyone standing still off it: onto it
        for p in self.people.iter_mut() {
            if matches!(p.place, Place::Ground) {
                if let Some(z) = world.walk_height_near(p.position.x, p.position.y, p.position.z) {
                    let d = z - p.position.z;
                    let still = p.vel.length() < 0.05;
                    if d.abs() < 3.0 && (d > 0.02 || (still && d.abs() > 0.02)) {
                        p.position.z = z;
                    }
                }
            }
        }
        self.populate_with(world, None, renderer, scene, center);
    }

    fn populate_with(
        &mut self,
        world: &World,
        net: Option<&Network>,
        _renderer: &Renderer,
        _scene: &mut Scene,
        center: DVec3,
    ) {
        self.center = center;
        let list: Vec<(i64, DVec3, f64, String)> = world
            .bus_stops
            .lock()
            .iter()
            .filter(|s| (s.1 - center).length() < STOP_RANGE + 100.0)
            .map(|s| (s.0, s.1, s.2, s.3.clone()))
            .collect();
        for (id, pos, rot, name) in list {
            // only once the ground under the stop is there
            if world.walk_height(pos.x, pos.y).is_none() || self.stops.contains_key(&id) {
                continue;
            }
            let st = self.build_pax_stop(world, net, id, pos, rot, &name);
            self.stops.insert(id, st);
        }
        self.started = true;
    }

    /// A stop as Omsi.exe sets it up (sub_620058, sub_620c0c, sub_61c604): its waiting
    /// places are the `[passpos]` of every object near it - within 10 m to the platform's
    /// side and from 10 m behind to the stop's length ahead of it -, the gather point a
    /// metre to the kerb and a metre ahead, the destinations of the trips leaving it.
    fn build_pax_stop(&mut self, world: &World, net: Option<&Network>, id: i64, pos: DVec3, heading: f64, name: &str) -> PaxStop {
        let length = world.stop_length(id);
        let side = world.stop_side(id).round().clamp(0.0, 255.0) as u8;
        let left = LEFT_HAND.load(std::sync::atomic::Ordering::Relaxed);
        let (xmax, xmin) = (if (side == 1) != left { 0.0 } else { 10.0 }, if (side == 0) != left { 0.0 } else { -10.0 });
        let h = heading.to_radians();
        let objects = world.object_positions.lock();
        let mut spots: Vec<WaitSpot> = Vec::new();
        for (obj, p, face, height) in world.waiting_places.lock().iter() {
            let Some((opos, _)) = objects.get(obj) else {
                if debug_pax() && (*p - pos).length() < 30.0 {
                    log::info!("stop {id}: waiting place of object {obj} at {p:?}: object position unknown");
                }
                continue;
            };
            // (sub_7f0db8 / sub_7f0d3c: the stop less the object)
            let v = pos - *opos;
            // (Omsi.exe looks at every object of the stop's tile and the ones round it; the
            // region below reaches the stop's length ahead, which a long bus station stop
            // takes past 40 m)
            if v.length() > 40.0_f64.max(length as f64 + 15.0) {
                continue;
            }
            // (0x7efb08 with -heading: in the stop's frame)
            let lat = h.cos() * v.x - h.sin() * v.y;
            let along = h.cos() * v.y + h.sin() * v.x;
            if !(-along < 10.0 && -along > -(length as f64).max(10.0) && -lat < xmax && -lat > xmin) {
                if debug_pax() && v.length() < 30.0 {
                    log::info!("stop {id}: object {obj} (waiting place {p:?}) not the stop's: across {:.1}, along {:.1}", -lat, -along);
                }
                continue;
            }
            spots.push(WaitSpot { pos: *p, face: *face, height: *height });
        }
        drop(objects);
        // the gather point (+0x48): (1, 0, 1) or (-1, 0, 1) through the stop's turn
        let x = if (side == 1) == left { 1.0 } else { -1.0 };
        let (fwd, right) = (DVec2::new(h.sin(), h.cos()), DVec2::new(h.cos(), -h.sin()));
        let g = pos.truncate() + right * x + fwd * 1.0;
        let gather = DVec3::new(g.x, g.y, pos.z);
        let lane = net.and_then(|n| self.ped.as_ref().and_then(|pn| pn.nearest(n, pos, 12.0))).map(|(l, s, _)| (l, s));
        let (enter_max, enter_min) = world.stop_enter(id);
        // the destinations: the stops the trips from here go on to, as likely as people
        // get off there; each with the termini of the buses that go there
        let lines: Vec<(String, HashSet<String>)> = self.stop_targets.as_ref().and_then(|m| m.get(&id)).cloned().unwrap_or_default();
        let weights: Vec<f32> = lines
            .iter()
            .map(|(n, _)| {
                world.bus_stops.lock().iter().find(|s| s.3.trim() == n.trim()).map(|s| world.stop_exit_weight(s.0)).unwrap_or(0.5)
            })
            .collect();
        let total: f32 = weights.iter().sum();
        let dests: Vec<(String, f32)> = if total > 0.0 {
            lines.iter().zip(&weights).map(|((n, _), w)| (n.clone(), w / total)).collect()
        } else {
            Vec::new()
        };
        if debug_pax() {
            log::info!("stop {id} '{name}' at ({:.1}, {:.1}, {:.2}) heading {heading:.0}: {} waiting places, length {length}, side {side}, {} destinations", pos.x, pos.y, pos.z, spots.len(), dests.len());
        }
        let n = spots.len();
        // what the timetable calls it - its id when the timetable does not know it, as the
        // targets then do
        let alias = match &self.stop_names {
            Some(n) => n.get(&id).cloned().unwrap_or_else(|| id.to_string()),
            None => String::new(),
        };
        PaxStop {
            name: name.to_string(),
            alias,
            pos,
            heading,
            gather,
            spots,
            taken: vec![false; n],
            enter_max,
            enter_min,
            length,
            lane,
            was_near: false,
            near: false,
            clock_ms: 0.0,
            want: 0,
            factor: 1.0,
            buses: Vec::new(),
            dests,
            lines,
        }
    }

    /// The stops near the player fill with people (sub_61bf94, every frame): a stop coming
    /// into range gets its people at once, one in range another one every 10..15 s while
    /// it has fewer than it should - its pass_enter mean times its own random factor
    /// times the passenger density, at most one per waiting place. A stop going out of
    /// range loses the people waiting there.
    fn stops_tick(&mut self, dt: f32, world: &World, renderer: &Renderer, scene: &mut Scene) {
        if self.mirror || self.avatar_only {
            return;
        }
        let mut ids: Vec<i64> = self.stops.keys().copied().collect();
        ids.sort_unstable();
        let forced = omsi_cfg::env::var("OMSI_PAX_WAITING").ok().and_then(|v| v.parse::<usize>().ok());
        // the people handed over to another player's bus stop counting once it has left
        // their stop (or the session)
        if !self.handed.is_empty() {
            let (stops, remote) = (&self.stops, &self.remote_now);
            self.handed.retain(|(stop, bus)| {
                let Some(s) = stops.get(stop) else { return false };
                remote.iter().any(|b| b.id == BusId::Ai(*bus) && (b.pos - s.pos).length() < 60.0)
            });
        }
        for id in ids {
            let center = self.center;
            let near = {
                let s = &self.stops[&id];
                (s.pos - center).length() < STOP_RANGE || self.lan_centers.iter().any(|c| (s.pos - *c).length() < STOP_RANGE)
            };
            let changed = {
                let s = self.stops.get_mut(&id).unwrap();
                s.was_near = s.near;
                s.near = near;
                s.was_near != s.near
            };
            if !near {
                if changed {
                    // (sub_61be80) the people waiting there go
                    for i in (0..self.people.len()).rev() {
                        let here = matches!(&self.people[i].state, State::Pax(p) if p.stop == Some(id) && p.inside.is_none() && matches!(p.task, Task::WaitingForBus | Task::WalkingToBusstop));
                        if here {
                            self.release(i);
                            let p = self.people.swap_remove(i);
                            self.retire(&p);
                        }
                    }
                    for t in self.stops.get_mut(&id).unwrap().taken.iter_mut() {
                        *t = false;
                    }
                }
                continue;
            }
            if changed {
                let r = self.rand_f() as f32;
                let s = self.stops.get_mut(&id).unwrap();
                let mean = (s.enter_max + s.enter_min) / 2.0;
                let k = if s.enter_max == 0.0 {
                    0.0
                } else if mean == 0.0 {
                    (s.enter_max - s.enter_min) / (s.enter_max * 2.0)
                } else {
                    (s.enter_max - s.enter_min) / (mean * 2.0)
                };
                s.factor = (r * 2.0 - 1.0) * k + 1.0;
            }
            let count = self.people.iter().filter(|p| matches!(&p.state, State::Pax(x) if x.stop == Some(id))).count() + self.handed.iter().filter(|h| h.0 == id).count();
            let want = {
                let s = &self.stops[&id];
                let mean = (s.enter_max + s.enter_min) / 2.0;
                // (0x61bf94: with a timetable, times the share of the trips due there - at a
                // stop no trip leaves from, nobody)
                let served = if self.stop_targets.is_some() && s.lines.is_empty() { 0.0 } else { 1.0 };
                let w = (self.density.max(0.0) * mean * s.factor * served).round().max(0.0) as usize;
                forced.unwrap_or(w).min(s.spots.len())
            };
            let s = self.stops.get_mut(&id).unwrap();
            s.want = want;
            s.clock_ms += dt * 1000.0;
            if !changed {
                let r = self.rand_f() as f32;
                if self.stops[&id].clock_ms <= r * 5000.0 + 10000.0 {
                    continue;
                }
            }
            let mut count = count;
            while count < want {
                if self.spawn_waiting(world, renderer, scene, id).is_none() {
                    break;
                }
                count += 1;
                if !changed {
                    break;
                }
            }
            if count >= want {
                self.stops.get_mut(&id).unwrap().clock_ms = 0.0;
            }
        }
    }

    /// A destination drawn from stop `id`'s (sub_61baa8): by weight; none when the weights
    /// leave the draw over. Also the stop's line record it matched.
    fn draw_dest(&mut self, id: i64) -> (Option<String>, Option<usize>) {
        let mut r = self.rand_f() as f32;
        let mut dest: Option<String> = None;
        let Some(stop) = self.stops.get(&id) else { return (None, None) };
        for (n, w) in &stop.dests {
            if r <= 0.0 {
                break;
            }
            r -= w;
            if r <= 0.0 {
                dest = Some(n.clone());
            }
        }
        let line = dest.as_ref().and_then(|d| stop.lines.iter().position(|(n, _)| n.trim() == d.trim()));
        (dest, line)
    }

    /// A person put at a free waiting place of stop `id` (sub_626044) with a destination
    /// drawn from the stop's (sub_61baa8); they settle there as task 6 does.
    fn spawn_waiting(&mut self, world: &World, renderer: &Renderer, scene: &mut Scene, id: i64) -> Option<usize> {
        if self.stops.get(&id)?.taken.iter().all(|t| *t) || !self.pool_room() {
            return None;
        }
        let k = self.take_spot(id)?;
        let sp = self.stops[&id].spots[k].clone();
        let (dest, line) = self.draw_dest(id);
        let walk = 1.1 + (self.rand_f() as f32 * 2.0 - 1.0) * 0.2;
        let mut pax = Pax::new(walk);
        pax.stop = Some(id);
        pax.spot = Some(k);
        pax.pos = sp.pos;
        pax.yaw = sp.face.to_radians();
        pax.dest = dest;
        pax.line = line;
        pax.st = 0;
        let Some(i) = self.spawn(world, renderer, scene, sp.pos, sp.face, State::Pax(Box::new(pax))) else {
            self.free_spot(id, k);
            return None;
        };
        let dummy_b: Vec<BusNow> = Vec::new();
        let dummy_ix: HashMap<BusId, usize> = HashMap::new();
        self.set_task(i, Task::WalkingToBusstop, &dummy_b, &dummy_ix, world);
        if debug_pax() {
            let d = self.pax(i).and_then(|p| p.dest.clone());
            log::info!("t={:.1} pax {} waits at stop {id} place {k}, for {:?}", self.time, self.people[i].label(), d);
        }
        Some(i)
    }

    /// Keep strollers on the pavements near the player, and people walking up to the stops.
    fn populate_on_foot(
        &mut self,
        world: &World,
        net: &Network,
        renderer: &Renderer,
        scene: &mut Scene,
        dt: f32,
    ) {
        let Some(ped) = self.ped.take() else { return };
        self.populate_on_foot_with(&ped, world, net, renderer, scene, dt);
        self.ped = Some(ped);
    }

    fn populate_on_foot_with(
        &mut self,
        ped: &PedNet,
        world: &World,
        net: &Network,
        renderer: &Renderer,
        scene: &mut Scene,
        _dt: f32,
    ) {
        let center = self.center;
        // strollers: as many as the pavement around carries
        let lanes: Vec<usize> = ped
            .ends
            .keys()
            .copied()
            .filter(|&l| {
                (net.lanes[l].start() - center).truncate().length() < STROLL_RADIUS * 0.9
                    && net.lanes[l].length() > 4.0
            })
            .collect();
        let crowd = (lanes.len() as f32 / 120.0).clamp(0.6, 3.0);
        let target =
            (self.pedestrians as f32 * crowd * self.density.clamp(0.0, 3.0)).round() as usize;
        // (0x62463c: walking the pavements only while fewer than half the pool do, and never
        // past the pool)
        let target = target.min(self.max_people / 2).min((self.max_people + self.people.iter().filter(|p| matches!(p.state, State::Strolling(_))).count()).saturating_sub(self.pool_used()));
        let have = self
            .people
            .iter()
            .filter(|p| matches!(p.state, State::Strolling(_)))
            // (around this player only, when a LAN host keeps people around several)
            .filter(|p| {
                self.lan_centers.is_empty() || (p.position - center).length() < STROLL_RADIUS
            })
            .count();
        if have < target && !lanes.is_empty() {
            for _ in 0..(target - have).min(4) {
                let lane = lanes[(self.rand() as usize) % lanes.len()];
                let len = net.lanes[lane].length();
                let s = (self.rand_f() as f32 * (len - 1.0)).max(0.5);
                let (p, h) = net.lanes[lane].at(s);
                if self.seen(p)
                    || (p - center).length() < 20.0
                    || (p - center).length() > STROLL_RADIUS * 0.9
                    || !world.has_ground(p.x, p.y)
                {
                    continue;
                }
                let fwd = self.rand_f() < 0.5;
                let leg = if fwd {
                    Leg { lane, a: s, b: len }
                } else {
                    Leg { lane, a: s, b: 0.0 }
                };
                let side = 0.3 + self.rand_f() as f32 * 0.4;
                let heading = if fwd { h as f64 } else { h as f64 + 180.0 };
                if let Some(i) = self.spawn(
                    world,
                    renderer,
                    scene,
                    p,
                    heading,
                    State::Strolling(PedWalk::new(vec![leg], true, side)),
                ) {
                    self.people[i].activity = Activity::Walk;
                }
            }
        }
        // OMSI_PAX_CROSS=x,y: a few pedestrians sent across the signalised crossing nearest that point
        if let Some((x, y)) = omsi_cfg::env::var("OMSI_PAX_CROSS").ok().and_then(|v| {
            let mut it = v.split(',').filter_map(|t| t.trim().parse::<f64>().ok());
            Some((it.next()?, it.next()?))
        }) {
            let want = DVec3::new(x, y, center.z);
            let placed = self
                .people
                .iter()
                .filter(|p| matches!(p.state, State::Strolling(ref w) if !w.roam || w.side < 0.0))
                .count();
            let lane = ped
                .ends
                .keys()
                .copied()
                .filter(|&l| net.lanes[l].traffic_light.is_some())
                .min_by(|a, b| {
                    (net.lanes[*a].start() - want)
                        .truncate()
                        .length()
                        .total_cmp(&(net.lanes[*b].start() - want).truncate().length())
                });
            if let (Some(cross), 0) = (lane, placed) {
                let (start_node, _) = ped.ends[&cross];
                let feeders: Vec<(usize, bool)> = ped.out[start_node]
                    .iter()
                    .copied()
                    .filter(|(l, _)| *l != cross)
                    .collect();
                log::info!(
                    "OMSI_PAX_CROSS: crossing path {cross} light {:?}, {} paths lead to it",
                    net.lanes[cross].traffic_light,
                    feeders.len()
                );
                for k in 0..6 {
                    let Some(&(lane, fwd)) = feeders.get(k % feeders.len().max(1)) else {
                        break;
                    };
                    let len = net.lanes[lane].length();
                    let back = (3.0 + k as f32 * 1.6).min(len);
                    let first = if fwd {
                        Leg {
                            lane,
                            a: back,
                            b: 0.0,
                        }
                    } else {
                        Leg {
                            lane,
                            a: len - back,
                            b: len,
                        }
                    };
                    let over = Leg {
                        lane: cross,
                        a: 0.0,
                        b: net.lanes[cross].length(),
                    };
                    let (p, h) = first.at(net, 0.0);
                    let mut walk = PedWalk::new(vec![first, over], true, 0.4);
                    // marked so that the knob spawns them once
                    walk.side = -0.4;
                    if let Some(i) =
                        self.spawn(world, renderer, scene, p, h, State::Strolling(walk))
                    {
                        self.people[i].activity = Activity::Walk;
                    }
                }
            }
        }
    }

    /// Whether the bus script reports `name`: it writes it (`PAX_*` are engine variables every
    /// vehicle has, so stock scripts set them without a varlist entry) or declares it.
    fn script_reports(v: &VehicleInstance, name: &str) -> bool {
        v.has_script_var(name) || v.ty.program.var(name).is_some_and(|id| v.ty.program.stores(id))
    }

    /// `PAX_Entry<i>_Open` / `PAX_Exit<i>_Open` as the bus script reports them. A bus whose
    /// script never sets them (or only sets some of them) falls back to its physical `door_<i>`
    /// or `door<i>` animations.
    fn doors_open(v: &VehicleInstance, n_entry: usize, n_exit: usize) -> (Vec<bool>, Vec<bool>) {
        let door_val = |k: usize| -> bool {
            v.var(&format!("door_{k}"))
                .or_else(|| v.var(&format!("door{k}")))
                .unwrap_or(0.0)
                > 0.5
        };
        // Exits in standard OMSI city buses (2 or more front door leaves) begin at door_2 (middle door),
        // while coaches with a single front door leaf begin at door_1. Exits must not be offset by
        // n_entry, because buses with all doors configured as entries (e.g. 3-door buses with 6 entries)
        // still place middle-door exits at door_2/3 and rear-door exits at door_4/5.
        let exit_door_base = if n_entry <= 1 { 1 } else { 2 };
        let entry: Vec<bool> = (0..n_entry)
            .map(|i| {
                let name = format!("PAX_Entry{i}_Open");
                if Self::script_reports(v, &name) {
                    v.var(&name).unwrap_or(0.0) > 0.5
                } else {
                    door_val(i.min(7))
                }
            })
            .collect();
        let exit: Vec<bool> = (0..n_exit)
            .map(|i| {
                let name = format!("PAX_Exit{i}_Open");
                if Self::script_reports(v, &name) {
                    v.var(&name).unwrap_or(0.0) > 0.5
                } else {
                    door_val((exit_door_base + i).min(7))
                }
            })
            .collect();
        (entry, exit)
    }


    /// The buses passengers deal with this frame.
    fn gather_buses(
        &mut self,
        world: &World,
        bus: Option<&VehicleInstance>,
        traffic: Option<&Traffic>,
    ) -> Vec<BusNow> {
        let mut out = Vec::new();
        let stops: Vec<(i64, DVec3, f64)> =
            world.bus_stops.lock().iter().map(|s| (s.0, s.1, s.2)).collect();
        // The stop a bus serves: the nearest in reach - but one facing the way the bus goes
        // before one facing the other way. The two stops of a street often lie within
        // reach of each other, and the people of the stop across the road then walked over
        // the carriageway, through the traffic, to a bus that was not theirs.
        let serving = |pos: DVec3, heading: f64, reach: f64| -> Option<i64> {
            stops
                .iter()
                .filter(|s| (s.1 - pos).length() < reach)
                .min_by(|a, c| {
                    let back = |s: &(i64, DVec3, f64)| crowd::angle_diff(heading, s.2).abs() > 100.0;
                    back(a)
                        .cmp(&back(c))
                        .then((a.1 - pos).length().total_cmp(&(c.1 - pos).length()))
                })
                .map(|s| s.0)
        };
        let bb_of = |v: &VehicleInstance| {
            let bb =
                v.ty.def
                    .bounding_box
                    .unwrap_or([2.5, 11.0, 3.0, 0.0, 0.0, 1.5]);
            (
                DVec2::new(bb[0] as f64 * 0.5, bb[1] as f64 * 0.5),
                DVec2::new(bb[3] as f64, bb[4] as f64),
            )
        };
        if let (Some(b), Some(cabin)) = (bus, self.player_cabin.clone()) {
            let speed = b.physics.velocity_kmh() as f64 / 3.6;
            let (entry_open, exit_open) =
                Self::doors_open(b, cabin.entries.len(), cabin.exits.len());
            let (half, centre) = bb_of(b);
            let trailers = part_frames(b, &cabin);
            let terminus = match (b.var("target_index_int"), b.host.hof.as_ref()) {
                (Some(i), Some(hof)) if i.is_finite() && i >= 0.0 => hof
                    .termini
                    .get(i.round() as usize)
                    .filter(|t| !t.all_exit)
                    .map(|t| t.texture_id.trim().to_string()),
                _ => None,
            };
            out.push(BusNow {
                terminus,
                id: BusId::Player,
                walk_open: None,
                cabin,
                pos: b.position,
                rot: b.body_rotation(),
                heading: b.heading,
                speed,
                entry_open,
                exit_open,
                interior: b.interior_light(),
                air: CabinAir::of(b),
                half,
                centre,
                accel: DVec2::ZERO,
                trailers,
            });
        }
        if let Some(t) = traffic {
            let near = self.center;
            let riding: HashSet<u64> = self
                .people
                .iter()
                .filter_map(|p| match p.state.bus() {
                    Some(BusId::Ai(id)) => Some(id),
                    _ => None,
                })
                .collect();
            let mut visits = HashMap::new();
            for c in t.cars.iter().filter(|c| c.is_bus()) {
                let from_eye = self.eye.map(|e| (c.vehicle.position - e.pos).length()).unwrap_or(f64::MAX);
                if (c.vehicle.position - near).length().min(from_eye) > 400.0 && !riding.contains(&c.id) {
                    continue;
                }
                let Some(cabin) = self.cabin_for(&c.vehicle) else {
                    continue;
                };
                let speed = c.state.speed as f64;
                let stop = if c.at_station() && speed.abs() < 0.3 {
                    serving(c.vehicle.position, c.vehicle.heading, 18.0)
                } else {
                    None
                };
                let since = match stop {
                    Some(s) => {
                        let v = match self.ai_visits.get(&c.id) {
                            Some(&(vs, t0)) if vs == s => (vs, t0),
                            _ => (s, self.time),
                        };
                        visits.insert(c.id, v);
                        self.time - v.1
                    }
                    None => 0.0,
                };
                let open = stop.is_some();
                let (mut entry_open, mut exit_open) = (
                    vec![false; cabin.entries.len()],
                    vec![false; cabin.exits.len()],
                );
                if open {
                    if Self::script_reports(&c.vehicle, "PAX_Entry0_Open") || c.vehicle.var("door_0").is_some() || c.vehicle.var("door0").is_some() {
                        let (e, x) =
                            Self::doors_open(&c.vehicle, cabin.entries.len(), cabin.exits.len());
                        entry_open = e;
                        exit_open = x;
                    } else if since > 2.5 {
                        // the script does not say: the doors are open while the bus boards
                        entry_open
                            .iter_mut()
                            .chain(exit_open.iter_mut())
                            .for_each(|o| *o = true);
                    }
                }
                self.seats
                    .entry(BusId::Ai(c.id))
                    .or_insert_with(|| vec![false; cabin.seats.len()]);
                let (half, centre) = bb_of(&c.vehicle);
                let trailers = part_frames(&c.vehicle, &cabin);
                out.push(BusNow {
                    terminus: c.bus.as_ref().map(|b| b.terminus.trim().to_string()).filter(|t| !t.is_empty()),
                    id: BusId::Ai(c.id),
                    walk_open: None,
                    cabin,
                    pos: c.vehicle.position,
                    rot: c.vehicle.body_rotation(),
                    heading: c.vehicle.heading,
                    speed,
                    entry_open,
                    exit_open,
                    interior: c.vehicle.interior_light(),
                    air: CabinAir::of(&c.vehicle),
                    half,
                    centre,
                    accel: DVec2::ZERO,
                    trailers,
                });
            }
            self.ai_visits = visits;
            let alive: HashSet<u64> = t.cars.iter().map(|c| c.id).chain(self.remote_now.iter().chain(self.placed_now.iter()).filter_map(|b| match b.id {
                BusId::Ai(id) => Some(id),
                BusId::Player => None,
            })).collect();
            self.seats.retain(|k, _| match k {
                BusId::Ai(id) => alive.contains(id),
                BusId::Player => true,
            });
        }
        for b in self.remote_now.iter().chain(self.placed_now.iter()) {
            self.seats.entry(b.id).or_insert_with(|| vec![false; b.cabin.seats.len()]);
            out.push(b.clone());
        }
        out
    }

    /// The vehicles standing in the world that the player placed and does not drive now:
    /// (their `Player::uid`, the vehicle). Their riders stay aboard; nobody new boards them.
    pub fn set_placed_buses<'a>(&mut self, buses: impl Iterator<Item = (u64, &'a VehicleInstance)>) {
        let mut out = Vec::new();
        for (uid, v) in buses {
            if let Some(b) = self.parked_bus(BusId::Ai(placed_bus_id(uid)), v) {
                out.push(b);
            }
        }
        self.placed_now = out;
    }



    /// A bus people may be in but do not board here (another player's, one the player left).
    fn parked_bus(&mut self, id: BusId, v: &VehicleInstance) -> Option<BusNow> {
        let cabin = self.cabin_for(v)?;
        let bb = v.ty.def.bounding_box.unwrap_or([2.5, 11.0, 3.0, 0.0, 0.0, 1.5]);
        let trailers = part_frames(v, &cabin);
        let walk_open = Self::doors_open(v, cabin.entries.len(), cabin.exits.len());
        Some(BusNow {
            terminus: None,
            id,
            entry_open: vec![false; cabin.entries.len()],
            exit_open: vec![false; cabin.exits.len()],
            walk_open: Some(walk_open),
            cabin,
            pos: v.position,
            rot: v.body_rotation(),
            heading: v.heading,
            speed: v.physics.velocity_kmh() as f64 / 3.6,
            interior: v.interior_light(),
            air: CabinAir::of(v),
            half: DVec2::new(bb[0] as f64 * 0.5, bb[1] as f64 * 0.5),
            centre: DVec2::new(bb[3] as f64, bb[4] as f64),
            accel: DVec2::ZERO,
            trailers,
        })
    }

    /// The player now drives vehicle `new_uid` and left `old_uid`: whoever rode in the one
    /// left stays in it (it is one of the placed vehicles now), whoever rode in the one
    /// taken over is the player's bus's, and the player's cabin is the new vehicle's own.
    /// (Riders followed the player into the next bus, and people boarding it took the old
    /// bus's seats - places in the air round a minibus's bonnet.)
    pub fn player_bus_swapped(&mut self, old_uid: u64, new_uid: u64, new_vehicle: &mut VehicleInstance) {
        let old = BusId::Ai(placed_bus_id(old_uid));
        let new = BusId::Ai(placed_bus_id(new_uid));
        // (through a free id: Player -> old, new -> Player)
        let tmp = BusId::Ai(u64::MAX);
        self.remap_bus(BusId::Player, tmp);
        self.remap_bus(new, BusId::Player);
        self.remap_bus(tmp, old);
        let kept = self.seats.remove(&BusId::Player);
        self.player_cabin = None;
        self.served_stop = None;
        self.set_cabin(new_vehicle);
        if let (Some(k), Some(now)) = (kept, self.seats.get_mut(&BusId::Player)) {
            if k.len() == now.len() {
                *now = k;
            }
        }
    }

    /// Bus `bus` is gone (the player removed it): whoever was in it stands where they were,
    /// on the ground, and walks off.
    pub fn evict(&mut self, bus: BusId, world: &World) {
        let _ = world;
        for i in (0..self.people.len()).rev() {
            let p = &self.people[i];
            let theirs = matches!(p.place, Place::Bus(b, _) if b == bus) || matches!(&p.state, State::Pax(x) if x.bus == Some(bus) || x.inside == Some(bus));
            if theirs {
                self.release(i);
                let p = self.people.swap_remove(i);
                self.retire(&p);
            }
        }
        self.seats.remove(&bus);
        self.bus_motion.remove(&bus);
        if bus == BusId::Player {
            self.player_cabin = None;
            self.served_stop = None;
        }
    }

    /// Everyone and everything that belongs to bus `from` belongs to `to` now.
    fn remap_bus(&mut self, from: BusId, to: BusId) {
        let fix = |b: &mut BusId| {
            if *b == from {
                *b = to;
            }
        };
        for p in &mut self.people {
            if let Place::Bus(b, _) = &mut p.place {
                fix(b);
            }
            if let State::Pax(x) = &mut p.state {
                if let Some(b) = x.bus.as_mut() {
                    fix(b);
                }
                if let Some(b) = x.inside.as_mut() {
                    fix(b);
                }
            }
        }
        if let Some(v) = self.seats.remove(&from) {
            self.seats.insert(to, v);
        }
        if let Some(v) = self.bus_motion.remove(&from) {
            self.bus_motion.insert(to, v);
        }
    }

    /// LAN play: the other players' buses this frame, by player id (their riders are drawn
    /// in them, see `remote_bus_id`).
    pub fn set_remote_buses<'a>(&mut self, buses: impl Iterator<Item = (u32, &'a VehicleInstance)>) {
        let mut out = Vec::new();
        for (player, v) in buses {
            let Some(cabin) = self.cabin_for(v) else { continue };
            let bb = v.ty.def.bounding_box.unwrap_or([2.5, 11.0, 3.0, 0.0, 0.0, 1.5]);
            let trailers = part_frames(v, &cabin);
            // (their doors as their game has them: a walker gets in only where one is open;
            // the passengers here never board it - that bus's own game boards them)
            let walk_open = Self::doors_open(v, cabin.entries.len(), cabin.exits.len());
            out.push(BusNow {
                terminus: None,
                id: BusId::Ai(remote_bus_id(player)),
                entry_open: vec![false; cabin.entries.len()],
                exit_open: vec![false; cabin.exits.len()],
                walk_open: Some(walk_open),
                cabin,
                pos: v.position,
                rot: v.body_rotation(),
                heading: v.heading,
                speed: v.physics.velocity_kmh() as f64 / 3.6,
                interior: v.interior_light(),
                air: CabinAir::of(v),
                half: DVec2::new(bb[0] as f64 * 0.5, bb[1] as f64 * 0.5),
                centre: DVec2::new(bb[3] as f64, bb[4] as f64),
                accel: DVec2::ZERO,
                trailers,
            });
        }
        self.remote_now = out;
    }

    /// Is `bus` among the buses people can be in this frame (an AI bus, or another player's)?
    pub fn knows_bus(&self, bus: u64) -> bool {
        self.remote_now.iter().any(|b| b.id == BusId::Ai(bus)) || self.seats.contains_key(&BusId::Ai(bus))
    }

    /// Is person `id` (one drawn for another game) here?
    pub fn has_mirror(&self, id: u32) -> bool {
        self.people.iter().any(|p| p.id == id && p.remote)
    }

    /// The riders of our own bus, for the other LAN players to see: (id, type, place in the
    /// bus frame, heading there, seat, activity).
    pub fn lan_riders(&self) -> Vec<LanPerson> {
        self.people
            .iter()
            .filter(|p| p.puppet.is_none() && !p.remote)
            .filter_map(|p| match p.place {
                Place::Bus(BusId::Player, l) => Some(LanPerson {
                    id: p.id,
                    ty: p.ty.clone(),
                    pos: p.position,
                    heading: p.heading,
                    speed: 0.0,
                    activity: p.activity,
                    aboard: Some((
                        0,
                        l,
                        p.lheading,
                        match &p.state {
                            State::Pax(x) if x.task == Task::SittingInBus => x.seat,
                            _ => None,
                        },
                    )),
                    waiting: None,
                }),
                _ => None,
            })
            .collect()
    }

    /// Nobody on foot walks into a wall: the scenery's collision boxes and meshes (shelters,
    /// fences, walls, buildings with a collision mesh) between knee and head height stop a
    /// step that would enter one, keeping the part of it along the wall. Somebody already
    /// inside one (a waiting place the map put in a shelter's box) is left alone - pushed
    /// out, they jumped. People used to walk through everything but the vehicles.
    fn keep_out_of_walls(&mut self, world: &World, who: &[usize], ground: &mut [(usize, Walker)]) {
        const R: f64 = 0.22;
        const CELL: f64 = 12.0;
        let collision = world.collision.lock();
        let places: Vec<DVec2> = world.waiting_places.lock().iter().map(|w| w.1.truncate()).collect();
        let (boxes, meshes, since) = (collision.boxes.len(), collision.meshes.len(), self.wall_key.3);
        if (boxes, meshes, places.len()) != (self.wall_key.0, self.wall_key.1, self.wall_key.2) || self.time - since > 2.0 || self.time < since {
            self.wall_cells.clear();
            self.wall_key = (boxes, meshes, places.len(), self.time);
        }
        let mut cells = std::mem::take(&mut self.wall_cells);
        for (k, w) in ground.iter_mut() {
            let i = who[*k];
            if w.fixed || self.people[i].place != Place::Ground {
                continue;
            }
            let p0 = self.people[i].position.truncate();
            if (w.pos - p0).length_squared() < 1e-8 {
                continue;
            }
            let z = self.people[i].position.z;
            let key = ((p0.x / CELL).floor() as i32, (p0.y / CELL).floor() as i32, z.floor() as i32);
            let walls = cells.entry(key).or_insert_with(|| {
                let c = DVec2::new((key.0 as f64 + 0.5) * CELL, (key.1 as f64 + 0.5) * CELL);
                let probe = omsi_sim::collision::Obb {
                    center: c,
                    half: DVec2::splat(CELL * 0.5 + 2.0),
                    heading: 0.0,
                    z0: key.2 as f64 - 1.0,
                    z1: key.2 as f64 + 3.5,
                    velocity: DVec2::ZERO,
                    mass: 0.0,
                    pole: None,
                    id: -1,
                };
                let near = collision.obstacles_near(&probe);
                let reach = near.iter().map(|o| (o.center - c).length() + o.half.length() + 1.0).fold(0.0, f64::max);
                let local: Vec<DVec2> = places.iter().copied().filter(|q| (*q - c).length() < reach).collect();
                near.into_iter()
                    .filter(|o| {
                        // a shelter given as one solid box has its waiting places inside:
                        // people go in there
                        let b = Block { center: o.center, half: o.half, heading: o.heading, vel: DVec2::ZERO };
                        !local.iter().any(|q| (*q - o.center).length() < o.half.length() + 1.0 && b.near(*q, 0.3))
                    })
                    .map(|o| {
                        (
                            Block {
                                center: o.center,
                                half: o.half + DVec2::splat(R),
                                heading: o.heading,
                                vel: DVec2::ZERO,
                            },
                            o.z0,
                            o.z1,
                        )
                    })
                    .collect()
            });
            for (b, z0, z1) in walls.iter() {
                // between the knees and the head of somebody standing here
                if *z0 > z + 1.6 || *z1 < z + 0.5 {
                    continue;
                }
                if (w.pos - b.center).length_squared() >= b.half.length_squared() || !b.near(w.pos, 0.0) || b.near(p0, -0.01) {
                    continue;
                }
                let (q, inside) = b.closest(w.pos);
                if !inside {
                    continue;
                }
                // onto the wall's face, keeping the step along it
                if omsi_cfg::env::var_os("OMSI_DEBUG_WALLS").is_some() {
                    log::info!("t={:.1} pax {} ({}) kept out of a wall ({:.1} x {:.1} m, heights {:.1}..{:.1}) at ({:.2}, {:.2}), its centre ({:.2}, {:.2}), want ({:.2}, {:.2}) vel ({:.2}, {:.2})", self.time, self.people[i].label(), self.people[i].state.name(), b.half.x * 2.0, b.half.y * 2.0, z0 - z, z1 - z, w.pos.x, w.pos.y, b.center.x, b.center.y, w.want.x, w.want.y, w.vel.x, w.vel.y);
                }
                let n = (q - w.pos).try_normalize().unwrap_or(DVec2::ZERO);
                w.pos = q + n * 0.005;
                let vn = w.vel.dot(n);
                if vn < 0.0 {
                    w.vel -= n * vn;
                }
                let fresh = self.people[i].detour <= 0.0;
                self.people[i].detour = 2.0;
                w.corridor = None;
                // walking straight at it: round it, the way that turns least from where they
                // want to go (a lamp post or a pillar stopped people dead)
                let speed = w.want.length();
                let t = DVec2::new(-n.y, n.x);
                if fresh || self.people[i].detour_side == 0.0 {
                    let along = w.want.dot(t);
                    self.people[i].detour_side = if along.abs() > 0.05 * speed {
                        along.signum()
                    } else if i % 2 == 0 {
                        1.0
                    } else {
                        -1.0
                    };
                }
                if speed > 0.2 && w.vel.dot(t) * self.people[i].detour_side < 0.4 * speed {
                    w.vel = t * self.people[i].detour_side * speed * 0.8;
                }
            }
        }
        self.wall_cells = cells;
    }


    /// Keep only the people the map's `humans.txt` names, an entry listed twice counting
    /// twice, as OMSI draws a map's pedestrians and passengers from that list alone. A map
    /// without the file, or whose list names nobody to be found, keeps everybody.
    fn use_map_humans(&mut self, world: &World) {
        if self.map_humans_done {
            return;
        }
        self.map_humans_done = true;
        let path = omsi_cfg::resolve_path(&world.map_dir, "humans.txt");
        let list = omsi_map::ailists::load_list(&path);
        if list.is_empty() {
            return;
        }
        // the people installed already (any content root, mods too), matched by the path
        // below `Humans/`; an entry not among them (a pack nested deeper than the scan) is
        // loaded from its own path
        let key = |p: &str| -> String {
            let p = p.replace('\\', "/").to_ascii_lowercase();
            match p.rfind("humans/") {
                Some(k) => p[k + 7..].to_string(),
                None => p,
            }
        };
        let mut picked: Vec<Arc<HumanType>> = Vec::new();
        for line in &list {
            let want = key(line.trim());
            match self.types.iter().find(|t| key(&t.def.path.to_string_lossy()) == want) {
                Some(t) => picked.push(t.clone()),
                None => picked.extend(map_human_types(&world.root, std::slice::from_ref(line))),
            }
        }
        // (a list that names nobody to be found keeps everybody: a map without people
        // looked broken)
        if picked.is_empty() {
            log::warn!("humans.txt of the map names nobody installed: keeping all people");
            return;
        }
        log::info!(
            "humans: {} of {} map entries loaded from {}",
            picked.len(),
            list.len(),
            path.display()
        );
        self.types = picked;
    }

    /// People the moving bus has just knocked down. OMSI counts them in the driver's
    /// personnel file; they are only counted once and then walk away.
    pub fn run_over(&mut self, bus: &VehicleInstance) -> u32 {
        if bus.physics.velocity_kmh().abs() < 5.0 {
            return 0;
        }
        let Some(bb) = bus.ty.def.bounding_box else {
            return 0;
        };
        let (half_x, half_y) = ((bb[0] - bb[3]).abs() / 2.0, (bb[1] - bb[4]).abs() / 2.0);
        let inv = bus.body_rotation().transpose();
        let mut knocked = Vec::new();
        for (i, p) in self.people.iter().enumerate() {
            // (sub_62a6a0 at 0x62dc6c: the people waiting at a stop and those on the pavements)
            let counts = match &p.state {
                State::Pax(x) => x.task == Task::WaitingForBus,
                State::Strolling(_) | State::Standing => true,
                State::Idle => false,
            };
            if p.place != Place::Ground || !counts {
                continue;
            }
            let local = inv.transform_vector3((p.position - bus.position).as_vec3());
            if local.x.abs() < half_x + 0.2 && local.y.abs() < half_y + 0.2 && local.z.abs() < 3.0 {
                knocked.push(i);
            }
        }
        let mut gone = Vec::new();
        for &i in knocked.iter().rev() {
            // a waiting passenger knocked down leaves the stop and walks off (sub_626818)
            if let State::Pax(x) = &self.people[i].state {
                let (at, h, stop) = (x.pos, x.yaw.to_degrees(), x.stop);
                self.release(i);
                let world = None::<&World>;
                let _ = world;
                self.walk_street_plain(i, at, h, stop, &mut gone);
            }
        }
        gone.sort_unstable();
        for i in gone.into_iter().rev() {
            let p = self.people.swap_remove(i);
            self.retire(&p);
        }
        knocked.len() as u32
    }

    /// `--riders n`: n passengers already in their places in the player's bus (a test
    /// start; OMSI's buses start empty), without a destination - they ride 1..20 km.
    pub fn seed_riders(&mut self, n: usize, bus: &VehicleInstance, world: &World, renderer: &Renderer, scene: &mut Scene) {
        let Some(cabin) = self.cabin_for(bus) else { return };
        let trailers = part_frames(bus, &cabin);
        let rot = bus.body_rotation();
        for _ in 0..n {
            let Some(k) = self.reserve_place(BusId::Player, cabin.seats.len()) else { break };
            let walk = 1.1 + (self.rand_f() as f32 * 2.0 - 1.0) * 0.2;
            let r = self.rand_f() as f32;
            let mut pax = Pax::new(walk);
            pax.bus = Some(BusId::Player);
            pax.inside = Some(BusId::Player);
            pax.seat = Some(k);
            pax.ride_km = r * 19.0 + 1.0;
            pax.task = Task::InBusToPlace;
            let at = train_point(bus.position, &rot, &trailers, cabin.seats[k].pos);
            let Some(i) = self.spawn(world, renderer, scene, at, bus.heading, State::Pax(Box::new(pax))) else {
                self.free_seat(BusId::Player, k);
                break;
            };
            let s = cabin.seats[k].clone();
            let seatheight = self.people[i].ty.def.seat_height;
            if let Some(p) = self.pax_mut(i) {
                p.task = Task::SittingInBus;
                p.st = 0;
                if s.seated {
                    p.seat_h = s.height;
                    p.pos = (s.pos - Vec3::Z * seatheight).as_dvec3();
                    p.pax_state = 2.0;
                } else {
                    p.pos = s.pos.as_dvec3();
                }
                p.yaw = (s.rot as f64).to_radians();
            }
            self.people[i].place = Place::Bus(BusId::Player, s.pos);
        }
    }

    /// Give back what a person holds (a waiting place, a seat) before they change plans.
    fn release(&mut self, i: usize) {
        let id = self.people[i].id;
        let State::Pax(x) = &mut self.people[i].state else { return };
        let (stop, spot, bus, seat) = (x.stop, x.spot.take(), x.bus.or(x.inside), x.seat.take());
        if let (Some(s), Some(k)) = (stop, spot) {
            self.free_spot(s, k);
        }
        if let (Some(b), Some(k)) = (bus, seat) {
            self.free_seat(b, k);
        }
        if self.desk_busy == Some(id) {
            self.desk_busy = None;
            self.request = None;
        }
    }

    /// OMSI_CHECK_WALLS: everybody inside a bus who stands away from its walkways (more
    /// than 0.45 m from every path link, not on a seat): through a seat back or a wall.
    fn check_walls(&self) {
        for p in &self.people {
            let Place::Bus(bus, local) = p.place else { continue };
            if matches!(&p.state, State::Pax(x) if x.task == Task::SittingInBus || x.st == 9) {
                continue;
            }
            let Some(bn) = self.last_buses.iter().find(|b| b.id == bus) else { continue };
            let pts = &bn.cabin.graph.points;
            let mut best = f32::INFINITY;
            for &(a, b, _) in &bn.cabin.links {
                let (Some(pa), Some(pb)) = (pts.get(a.max(0) as usize), pts.get(b.max(0) as usize)) else { continue };
                let ab = *pb - *pa;
                let t = if ab.length_squared() > 1e-6 { ((local - *pa).dot(ab) / ab.length_squared()).clamp(0.0, 1.0) } else { 0.0 };
                let q = *pa + ab * t;
                best = best.min((q.truncate() - local.truncate()).length() + (q.z - local.z).abs());
            }
            let near_seat = bn.cabin.seats.iter().any(|s| (s.floor - local).truncate().length() < 0.35 || (s.pos - local).truncate().length() < 0.35);
            if best > 0.45 && !near_seat && !bn.cabin.links.is_empty() {
                log::warn!("t={:.1} person {} in bus {:?} off the walkways by {best:.2} m at ({:.2}, {:.2}, {:.2}), state {}", self.time, p.id, bus, local.x, local.y, local.z, p.state.name());
            }
        }
    }

    /// Report the waiting and alighting passengers to the bus script, the way OMSI does.
    pub fn write_pax_vars(&self, b: &mut VehicleInstance) {
        for (i, r) in self.entry_req.iter().enumerate() {
            b.set_var(&format!("PAX_Entry{i}_Req"), if *r { 1.0 } else { 0.0 });
        }
        for (i, r) in self.exit_req.iter().enumerate() {
            b.set_var(&format!("PAX_Exit{i}_Req"), if *r { 1.0 } else { 0.0 });
        }
    }

    /// Who wants a timetable bus to stop, as Omsi.exe asks before it lets one pull in
    /// (0x7da91f): the AI buses with somebody aboard on the way to a door to get off
    /// (task 5), and the stops where somebody is waiting for a bus or walking to one
    /// (tasks 1 to 3).
    pub fn stop_wishes(&self) -> (HashSet<u64>, HashSet<i64>) {
        let (mut alighting, mut waiting) = (HashSet::new(), HashSet::new());
        for p in &self.people {
            let State::Pax(x) = &p.state else { continue };
            match x.task {
                Task::InBusToExit => {
                    if let Some(BusId::Ai(id)) = x.inside {
                        alighting.insert(id);
                    }
                }
                Task::WaitingForBus | Task::ToBus | Task::WalkingToBus => {
                    if let Some(s) = x.stop {
                        waiting.insert(s);
                    }
                }
                _ => {}
            }
        }
        (alighting, waiting)
    }

    /// Timetable buses to hold at their stop, for the traffic.
    pub fn take_holds(&mut self) -> Vec<(u64, Option<i64>, f32)> {
        std::mem::take(&mut self.holds)
    }

    /// Door requests for the timetable buses, for the traffic to hand to their scripts.
    pub fn take_ai_requests(&mut self) -> Vec<(u64, Vec<bool>, Vec<bool>)> {
        std::mem::take(&mut self.ai_requests)
    }

    /// A line for the HUD about something that just happened.
    pub fn take_message(&mut self) -> Option<String> {
        self.message.take()
    }

    /// What the driver should do now, for the HUD: a passenger waiting at the cash desk
    /// for the ticket (only when the driver has to sell it).
    pub fn hint(&self) -> Option<String> {
        if !self.boarding.eq_ignore_ascii_case("pay") {
            return None;
        }
        self.people.iter().find(|p| matches!(&p.state, State::Pax(x) if x.inside == Some(BusId::Player) && x.ticket == TICKET_BUY && x.sub == 5))?;
        let (name, value) = self.request.clone()?;
        Some(format!("Passenger waiting for a ticket: {name} {value:.2} - press {}", self.ticket_key))
    }

    /// People sitting on each `[passpos]` of the player's bus, for `GetHumanCountOnSeat`
    /// (the BVG Citaro folds its tip-up seats down when somebody sits on them).
    pub fn seat_counts(&self) -> Vec<u32> {
        let Some(cabin) = self.player_cabin.as_ref() else {
            return Vec::new();
        };
        let sitting = self.people.iter().filter_map(|p| match &p.state {
            State::Pax(x) if x.inside == Some(BusId::Player) && x.task == Task::SittingInBus => x.seat,
            _ => None,
        });
        seat_numbers(&cabin.seats, sitting)
    }

    /// How many people stand on each `paths.cfg` link inside the player's bus, for the
    /// scripts' `GetHumanCountOnPathLink` (the NL/NG uses it for the fare gate).
    pub fn path_link_counts(&self) -> Vec<u32> {
        let Some(cabin) = self.player_cabin.as_ref() else {
            return Vec::new();
        };
        // (the link each walker is on, +0x698)
        let mut out = vec![0u32; cabin.links.len()];
        for p in &self.people {
            if let State::Pax(x) = &p.state {
                if x.inside == Some(BusId::Player) && (x.st == 1 || x.st == 5 || x.st == 9) {
                    if let Some(c) = x.link.and_then(|l| out.get_mut(l)) {
                        *c += 1;
                    }
                }
            }
        }
        out
    }

    /// Where everybody is, for logs.
    pub fn positions(&self) -> Vec<(String, DVec3)> {
        self.people
            .iter()
            .map(|p| (p.state.name().to_string(), p.position))
            .collect()
    }

    /// Count of people per state, for logs.
    pub fn summary(&self) -> String {
        let mut counts: std::collections::BTreeMap<&'static str, usize> = Default::default();
        for p in &self.people {
            *counts.entry(p.state.name()).or_default() += 1;
        }
        let mut out = counts
            .iter()
            .map(|(k, v)| format!("{v} {k}"))
            .collect::<Vec<_>>()
            .join(", ");
        let (n, total, worst) = self.tick_stats;
        if n > 0 {
            out.push_str(&format!(
                "; {:.2} ms a frame, longest {worst:.1} ms",
                total / n as f64
            ));
        }
        let (frames, posed, ms, up) = self.pose_stats;
        if frames > 0 {
            out.push_str(&format!(
                "; posing {:.2} ms a frame ({:.1} people, {:.2} ms of it uploading and placing)",
                ms / frames as f64,
                posed as f64 / frames as f64,
                up / frames as f64
            ));
        }
        out
    }

    /// Advance everybody. `bus`: the player's vehicle; `traffic`: the timetable buses,
    /// the traffic lights and the cars pedestrians wait for. Returns true when a passenger
    /// took the printed ticket (the caller resets `GivenTicket`).
    pub fn tick(
        &mut self,
        dt: f32,
        world: &World,
        bus: Option<&VehicleInstance>,
        traffic: Option<&Traffic>,
        renderer: &Renderer,
        scene: &mut Scene,
    ) -> bool {
        let started = std::time::Instant::now();
        self.tick_stages.clear();
        let took = self.tick_inner(dt, world, bus, traffic, renderer, scene);
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        if (debug_pax() || omsi_cfg::env::var_os("OMSI_PROFILE").is_some()) && ms > 30.0 {
            log::info!(
                "t={:.1} slow people tick: {ms:.1} ms ({} people): {}",
                self.time,
                self.people.len(),
                self.tick_stages.iter().filter(|s| s.1 >= 1.0).map(|(n, t)| format!("{n} {t:.1}")).collect::<Vec<_>>().join(", ")
            );
        }
        if omsi_cfg::env::var_os("OMSI_CHECK_WALLS").is_some() {
            self.check_walls();
        }
        // OMSI_CHECK_GROUND=1: people on foot with a walkable surface over their heads'
        // reach above them, every two seconds (people "in the ground")
        if omsi_cfg::env::var_os("OMSI_CHECK_GROUND").is_some() && (self.time / 2.0).floor() != ((self.time - dt as f64) / 2.0).floor() {
            for p in &self.people {
                if !matches!(p.place, Place::Ground) || p.puppet.is_some() {
                    continue;
                }
                // the floor under the feet: the highest face up to a step (0.5 m) over them
                let floor = world.walk_height_near(p.position.x, p.position.y, p.position.z);
                if let Some(f) = floor {
                    if f - p.position.z > 0.05 {
                        let detail = match &p.state {
                            State::Pax(x) => format!(" st {} pax_state {} task {:?} pos.z {:.2}", x.st, x.pax_state, x.task, x.pos.z),
                            _ => String::new(),
                        };
                        log::warn!("t={:.1} person {} ({}) {:.2} m under the floor at ({:.1}, {:.1}, {:.2}), top surface {:?}{detail}", self.time, p.id, p.state.name(), f - p.position.z, p.position.x, p.position.y, p.position.z, world.walk_height(p.position.x, p.position.y));
                    }
                }
            }
        }
        self.tick_stats.0 += 1;
        self.tick_stats.1 += ms;
        self.tick_stats.2 = self.tick_stats.2.max(ms);
        took
    }

    #[allow(unused_assignments)]
    fn tick_inner(
        &mut self,
        dt: f32,
        world: &World,
        bus: Option<&VehicleInstance>,
        traffic: Option<&Traffic>,
        renderer: &Renderer,
        scene: &mut Scene,
    ) -> bool {
        let mut mark = std::time::Instant::now();
        macro_rules! stage {
            ($name:expr) => {{
                let now = std::time::Instant::now();
                self.tick_stages.push(($name, (now - mark).as_secs_f64() * 1000.0));
                mark = now;
            }};
        }
        self.use_map_humans(world);
        self.time += dt as f64;
        let net = traffic.map(|t| &t.net);
        if let Some(b) = bus {
            self.center = b.position;
        } else if let Some(e) = self.eye {
            self.center = e.pos;
        }
        let generation = world.tiles_generation.load(std::sync::atomic::Ordering::Relaxed);
        if generation != self.tiles_seen {
            self.tiles_seen = generation;
            self.tiles_changed(world);
        }
        stage!("tiles");
        // tiles brought lanes: their pavements join the network, and stops without one look again
        if let (Some(pn), Some(n)) = (self.ped.as_mut(), net) {
            if pn.built < n.lanes.len() {
                let added = pn.extend(n);
                if added > 0 {
                    let ids: Vec<(i64, DVec3)> = self.stops.iter().filter(|(_, s)| s.lane.is_none()).map(|(k, s)| (*k, s.pos)).collect();
                    for (id, pos) in ids {
                        let lane = self.ped.as_ref().and_then(|pn| pn.nearest(n, pos, 12.0)).map(|(l, s, _)| (l, s));
                        self.stops.get_mut(&id).unwrap().lane = lane;
                    }
                }
            }
        }
        if self.ped.is_none() {
            if let Some(n) = net {
                self.ped = Some(PedNet::build(n));
                let ids: Vec<(i64, DVec3)> = self.stops.iter().map(|(k, s)| (*k, s.pos)).collect();
                for (id, pos) in ids {
                    let lane = self.ped.as_ref().and_then(|pn| pn.nearest(n, pos, 12.0)).map(|(l, s, _)| (l, s));
                    self.stops.get_mut(&id).unwrap().lane = lane;
                }
            }
        }
        stage!("pedestrian network");
        if let Some(n) = net {
            self.stroll_timer -= dt;
            if self.stroll_timer <= 0.0 {
                self.stroll_timer = 1.0;
                let c = self.center;
                self.populate_with(world, Some(n), renderer, scene, c);
                if !self.mirror {
                    self.populate_on_foot(world, n, renderer, scene, 1.0);
                    self.populate_lan_centers(world, n, renderer, scene);
                }
            }
        }
        stage!("populate");
        let mut buses = self.gather_buses(world, bus, traffic);
        for b in &buses {
            if b.entry_open.iter().chain(b.exit_open.iter()).any(|o| *o) {
                self.last_door_open.insert(b.id, self.time);
            }
        }
        // how the floor of each bus accelerates (for the drawing of its riders)
        if dt > 1e-4 {
            let mut motion = HashMap::new();
            for bn in buses.iter_mut() {
                let accel = match self.bus_motion.get(&bn.id) {
                    Some(&(v0, h0, a0)) => {
                        let yaw_rate = crowd::angle_diff(h0, bn.heading).to_radians() / dt as f64;
                        let raw = DVec2::new(bn.speed * yaw_rate, (bn.speed - v0) / dt as f64).clamp(DVec2::splat(-6.0), DVec2::splat(6.0));
                        a0 + (raw - a0) * (1.0 - (-(dt as f64) / 0.2).exp())
                    }
                    None => DVec2::ZERO,
                };
                bn.accel = accel;
                motion.insert(bn.id, (bn.speed, bn.heading, accel));
            }
            self.bus_motion = motion;
        }
        let buses = buses;
        self.last_buses = buses.clone();
        let bus_ix: HashMap<BusId, usize> = buses.iter().enumerate().map(|(i, b)| (b.id, i)).collect();
        // the stops: which buses stand at them (sub_61f93c), who waits there (sub_61bf94)
        let at_stops = self.register_buses(&buses, dt);
        self.claim_waiting();
        self.ride_comfort(dt, bus, &buses, &bus_ix, world);
        if !self.avatar_only {
            self.stops_tick(dt, world, renderer, scene);
        }
        stage!("stops");
        // the passengers (sub_6ffc7c)
        let mut taken_ticket = false;
        let mut remove: Vec<usize> = Vec::new();
        self.pax_frame(dt, world, &buses, &bus_ix, &at_stops, bus, renderer, scene, &mut taken_ticket, &mut remove);
        stage!("passengers");
        // the pedestrians: a crowd on the pavements
        let mut cars: Vec<(DVec2, DVec2, f64)> = Vec::new();
        let mut blocks: Vec<Block> = Vec::new();
        if let Some(t) = traffic {
            for c in &t.cars {
                if (c.vehicle.position - self.center).length() > 320.0 {
                    continue;
                }
                let h = c.vehicle.heading.to_radians();
                let fwd = DVec2::new(h.sin(), h.cos());
                let bb = c.vehicle.ty.def.bounding_box.unwrap_or([2.0, 4.5, 1.6, 0.0, 0.0, 0.8]);
                cars.push((c.vehicle.position.truncate(), fwd * c.state.speed as f64, bb[1] as f64 * 0.5));
                if !matches!(c.vehicle.ty.def.kind, omsi_vehicle::VehicleKind::Other(3)) {
                    let o = omsi_sim::collision::Obb::from_box(bb, c.vehicle.position, c.vehicle.heading);
                    blocks.push(Block { center: o.center, half: o.half, heading: o.heading, vel: fwd * c.state.speed as f64 });
                    for t in &c.vehicle.trailers {
                        let tb = t.ty.def.bounding_box.unwrap_or([2.5, 7.0, 3.0, 0.0, 0.0, 1.5]);
                        let o = omsi_sim::collision::Obb::from_box(tb, t.position, t.heading);
                        let th = t.heading.to_radians();
                        blocks.push(Block { center: o.center, half: o.half, heading: o.heading, vel: DVec2::new(th.sin(), th.cos()) * c.state.speed as f64 });
                    }
                }
            }
        }
        for o in world.parked_boxes.lock().iter() {
            if (o.center - self.center.truncate()).length() < 320.0 {
                blocks.push(Block { center: o.center, half: o.half, heading: o.heading, vel: DVec2::ZERO });
            }
        }
        if let Some(pb) = bus_ix.get(&BusId::Player).map(|&i| &buses[i]) {
            cars.push((pb.pos.truncate(), pb.fwd() * pb.speed, pb.half.y));
            for t in &pb.trailers {
                let h = t.heading.to_radians();
                cars.push((t.pos.truncate(), DVec2::new(h.sin(), h.cos()) * pb.speed, t.half.y));
            }
            blocks.extend(pb.blocks());
        }
        let mut wants: Vec<Want> = Vec::with_capacity(self.people.len());
        for i in 0..self.people.len() {
            self.people[i].t_state += dt;
            let w = if self.people[i].puppet.is_some() || matches!(self.people[i].state, State::Pax(_)) {
                Want::stand(None, Activity::Stand)
            } else if self.people[i].remote {
                self.mirror_want(i, &buses)
            } else {
                self.decide(i, dt, world, net, traffic, &cars, &mut remove)
            };
            wants.push(w);
        }
        // a standing vehicle in the way: wait, then go round it
        for i in 0..self.people.len() {
            let p = &self.people[i];
            if remove.contains(&i) || p.puppet.is_some() || p.remote || !matches!(p.state, State::Strolling(_)) {
                continue;
            }
            let want = wants[i].vel;
            let speed = want.length();
            if speed < 0.2 {
                self.people[i].car_wait = 0.0;
                continue;
            }
            let ahead = p.position.truncate() + want / speed * 0.9;
            let in_way = blocks.iter().any(|b| {
                b.vel.length() < 0.5 && b.near(ahead, BODY_OUTSIDE + 0.15) && {
                    let (q, inside) = b.closest(ahead);
                    inside || (ahead - q).length() < BODY_OUTSIDE + 0.15
                }
            });
            if !in_way {
                self.people[i].car_wait = 0.0;
                continue;
            }
            self.people[i].car_wait += dt;
            if self.people[i].car_wait > 8.0 {
                self.people[i].detour = self.people[i].detour.max(4.0);
                self.people[i].car_wait = 0.0;
            } else if self.people[i].detour <= 0.0 {
                wants[i].vel = DVec2::ZERO;
            }
        }
        // the crowd of the pavements (passengers outside stand in it as they are)
        let mut walkers: Vec<Walker> = Vec::with_capacity(self.people.len());
        let mut who: Vec<usize> = Vec::with_capacity(self.people.len());
        for (i, p) in self.people.iter().enumerate() {
            if remove.contains(&i) || p.puppet.is_some() || p.remote || p.place != Place::Ground {
                continue;
            }
            let fixed = matches!(p.state, State::Pax(_));
            let w = &wants[i];
            walkers.push(Walker {
                pos: p.position.truncate(),
                vel: p.vel,
                radius: BODY_OUTSIDE,
                want: w.vel,
                give: w.give,
                space: 0,
                fixed,
                ghost: p.ghost > 0.0,
                corridor: if p.detour > 0.0 { None } else { w.corridor },
            });
            who.push(i);
        }
        let near_blocks: Vec<Block> = blocks.into_iter().filter(|b| walkers.iter().any(|w| b.near(w.pos, 25.0))).collect();
        let mut g = walkers.clone();
        crowd::step(&mut g, &near_blocks, &CrowdParams::default(), dt as f64);
        let mut ground: Vec<(usize, Walker)> = g.into_iter().enumerate().collect();
        self.keep_out_of_walls(world, &who, &mut ground);
        let mut moved = vec![false; self.people.len()];
        for (k, w) in ground {
            let i = who[k];
            if matches!(self.people[i].state, State::Pax(_)) {
                continue;
            }
            moved[i] = true;
            self.apply(i, &w, &wants[i], dt, world, net, &buses, &bus_ix);
        }
        for i in 0..self.people.len() {
            if !moved[i] && !matches!(self.people[i].state, State::Pax(_)) {
                self.carry(i, dt, &buses, &bus_ix, wants[i].face);
            }
        }
        self.animate(dt, world, &buses, &bus_ix);
        remove.sort_unstable();
        remove.dedup();
        for i in remove.into_iter().rev() {
            self.release(i);
            let p = self.people.swap_remove(i);
            if debug_pax() {
                log::info!("t={:.1} pax {} taken away ({}){}", self.time, p.label(), p.state.name(), if self.seen(p.position) { " IN SIGHT" } else { "" });
            }
            self.retire(&p);
        }
        self.give_ticket = false;
        stage!("pedestrians");
        taken_ticket
    }

    /// What pedestrian `i` wants this frame (task 8, `WalkStreet`).
    #[allow(clippy::too_many_arguments)]
    fn decide(
        &mut self,
        i: usize,
        dt: f32,
        world: &World,
        net: Option<&Network>,
        traffic: Option<&Traffic>,
        cars: &[(DVec2, DVec2, f64)],
        remove: &mut Vec<usize>,
    ) -> Want {
        let state = self.people[i].state.clone();
        let pos2 = self.people[i].position.truncate();
        // somebody walking on towards ground that is not loaded goes
        if self.people[i].place == Place::Ground && self.people[i].vel.length_squared() > 1e-4 && !world.has_ground(pos2.x, pos2.y) {
            remove.push(i);
            return Want::stand(None, Activity::Stand);
        }
        match state {
            State::Strolling(mut walk) => {
                let seen = self.seen(self.people[i].position);
                // (a stroller goes only once well out of everybody's range and out of sight)
                let far = self.far_from_players(self.people[i].position, STROLL_RADIUS * 2.0);
                let Some(net) = net else {
                    remove.push(i);
                    return Want::stand(None, Activity::Stand);
                };
                if far && !seen {
                    remove.push(i);
                    return Want::stand(None, Activity::Stand);
                }
                let w = self.walk_want(i, &mut walk, net, traffic, cars, dt);
                self.people[i].state = State::Strolling(walk);
                w
            }
            State::Standing => {
                if !self.seen(self.people[i].position) && self.far_from_players(self.people[i].position, STROLL_RADIUS) {
                    remove.push(i);
                }
                Want::stand(None, Activity::Stand)
            }
            _ => Want::stand(None, Activity::Stand),
        }
    }








    /// Where a walk along the pavement takes somebody next.
    fn walk_want(
        &mut self,
        i: usize,
        walk: &mut PedWalk,
        net: &Network,
        traffic: Option<&Traffic>,
        cars: &[(DVec2, DVec2, f64)],
        dt: f32,
    ) -> Want {
        let mut ped = self.ped.take();
        let w = self.walk_want_with(ped.as_mut(), i, walk, net, traffic, cars, dt);
        self.ped = ped;
        w
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_want_with(
        &mut self,
        mut ped: Option<&mut PedNet>,
        i: usize,
        walk: &mut PedWalk,
        net: &Network,
        traffic: Option<&Traffic>,
        cars: &[(DVec2, DVec2, f64)],
        dt: f32,
    ) -> Want {
        let pos2 = self.people[i].position.truncate();
        let pace = self.people[i].pace;
        if walk.leg >= walk.legs.len() {
            return Want::stand(None, Activity::Stand);
        }
        let leg = walk.legs[walk.leg];
        if walk.s >= leg.len() - 0.35 || walk.held > 0.0 {
            // at the end of the leg: which way on
            if walk.leg + 1 >= walk.legs.len() {
                if !walk.roam {
                    walk.leg += 1;
                    return Want::stand(None, Activity::Stand);
                }
                let pick = self.rand();
                let next = ped
                    .as_ref()
                    .and_then(|p| {
                        p.end_node(net, &leg)
                            .and_then(|n| p.next_leg(net, n, leg.lane, pick))
                    })
                    .unwrap_or_else(|| {
                        // a leg that ends in the middle of its path (the point of a stop
                        // somebody got off at) goes on to one of its ends: turned round
                        // there, the people off a bus were sent back to the same point
                        // every frame and milled round each other at the stop (#913)
                        let lane_len = net.lanes[leg.lane].length();
                        if leg.b > 0.05 && leg.b < lane_len - 0.05 {
                            let fwd = if leg.len() > 0.05 { leg.b > leg.a } else { pick % 2 == 0 };
                            Leg {
                                lane: leg.lane,
                                a: leg.b,
                                b: if fwd { lane_len } else { 0.0 },
                            }
                        } else {
                            Leg {
                                lane: leg.lane,
                                a: leg.b,
                                b: leg.a,
                            }
                        }
                    });
                walk.legs.push(next);
                if walk.leg > 6 {
                    walk.legs.drain(..walk.leg);
                    walk.leg = 0;
                }
            }
            let next = walk.legs[walk.leg + 1];
            match self.may_cross(
                ped.as_deref_mut(),
                net,
                &next,
                traffic,
                cars,
                pace,
                walk.held,
            ) {
                Ok(()) => {
                    if walk.held > 0.0 && debug_pax() {
                        let light = net.lanes[next.lane].traffic_light.and_then(|(c, li)| {
                            traffic
                                .and_then(|t| t.light_state(c, li))
                                .map(|(st, left)| {
                                    format!(", light {c}.{li} state {st} for {left:.1} s more")
                                })
                        });
                        log::info!(
                            "t={:.1} pax {} crosses path {} after waiting {:.0} s{}",
                            self.time,
                            self.people[i].label(),
                            next.lane,
                            walk.held,
                            light.unwrap_or_default()
                        );
                    }
                    walk.s = (walk.s - leg.len()).max(0.0);
                    walk.leg += 1;
                    walk.held = 0.0;
                }
                Err(why) => {
                    walk.held += dt;
                    self.people[i].why = why;
                    // a light that stays red (nobody crosses on red any more): a stroller
                    // gives up after three minutes and walks back the way they came
                    if walk.roam && why == "red light" && walk.held > 180.0 {
                        if debug_pax() {
                            log::info!(
                                "t={:.1} pax {} gives up waiting at the red light and turns back",
                                self.time,
                                self.people[i].label()
                            );
                        }
                        walk.legs.truncate(walk.leg + 1);
                        walk.legs.push(Leg {
                            lane: leg.lane,
                            a: leg.b,
                            b: leg.a,
                        });
                        walk.held = 0.0;
                        return Want::stand(None, Activity::Stand);
                    }
                    // at the kerb, facing the way across, spread along it and a step back
                    let (end, _) = leg.at(net, leg.len());
                    let (_, h) = next.at(net, 0.3);
                    let hr = h.to_radians();
                    let (fwd, right) = (
                        DVec2::new(hr.sin(), hr.cos()),
                        DVec2::new(hr.cos(), -hr.sin()),
                    );
                    let id = self.people[i].id;
                    let spread = ((id % 5) as f64 - 2.0) * 0.45;
                    let back = 0.25 + (id % 3) as f64 * 0.45;
                    let spot = end.truncate() + right * spread - fwd * back;
                    return Want {
                        vel: arrive(pos2, spot, pace * 0.6),
                        face: Some(h),
                        give: 0.5,
                        corridor: None,
                        idle: Activity::Stand,
                    };
                }
            }
        }
        let leg = walk.legs[walk.leg];
        let len = leg.len();
        let (p, h) = leg.at(net, (walk.s + 1.3).min(len));
        let lane = &net.lanes[leg.lane];
        let width = (lane.width as f64).max(1.0);
        let crossing = lane.traffic_light.is_some()
            || ped
                .as_ref()
                .map(|p| {
                    p.crossings
                        .get(&leg.lane)
                        .map(|x| !x.is_empty())
                        .unwrap_or(false)
                })
                .unwrap_or(false);
        // keep to the right of the pavement (less so on a crossing) - the left where the
        // traffic drives on the left
        let side = if crossing {
            (walk.side.abs() as f64).min(0.3)
        } else {
            (walk.side.abs() as f64).min(width * 0.5 - 0.3).max(0.0)
        };
        let side = if net.left_hand { -side } else { side };
        let hr = h.to_radians();
        let right = DVec2::new(hr.cos(), -hr.sin());
        let target = p.truncate() + right * side;
        let vel = (target - pos2).normalize_or_zero() * pace;
        // stay on the path: the lane locally, as wide as it is
        let (a, _) = leg.at(net, (walk.s - 2.0).max(0.0));
        let (b, _) = leg.at(net, (walk.s + 2.5).min(len));
        let (m, _) = leg.at(net, (walk.s + 0.25).min(len));
        let bow = crowd::project_on_segment(m.truncate(), a.truncate(), b.truncate())
            .0
            .distance(m.truncate());
        let corridor = ((b - a).truncate().length() > 0.5).then(|| {
            (
                a.truncate() + right * side,
                b.truncate() + right * side,
                (width * 0.5 - side).max(0.35) + bow,
            )
        });
        let corridor = path_corridor(pos2, corridor);
        if self.people[i].why != "queueing behind somebody" {
            self.people[i].why = "";
        }
        Want {
            vel,
            face: None,
            give: 1.0,
            corridor,
            idle: Activity::Stand,
        }
    }

    /// May a pedestrian at the kerb start along `next`? A pedestrian light must show green,
    /// and the time left to get across - the green and then the clearance until a light of
    /// the carriageway turns green - must do; without a light no car may be about to pass
    /// the crossing. Somebody who has waited very long takes any green (never a red).
    #[allow(clippy::too_many_arguments)]
    fn may_cross(
        &self,
        mut ped: Option<&mut PedNet>,
        net: &Network,
        next: &Leg,
        traffic: Option<&Traffic>,
        cars: &[(DVec2, DVec2, f64)],
        pace: f64,
        held: f32,
    ) -> Result<(), &'static str> {
        if !next.from_end(net) {
            return Ok(());
        }
        let lane = &net.lanes[next.lane];
        let t_cross = next.len() as f64 / pace.max(0.5) + 1.0;
        if let (Some((c, li)), Some(t)) = (lane.traffic_light, traffic) {
            if let Some((state, left)) = t.light_state(c, li) {
                if !omsi_sim::traffic::TrafficLightController::allows_go(state) {
                    return Err("red light");
                }
                if held > 150.0 {
                    return Ok(());
                }
                // A pedestrian green is short (8 s at Grundorf for an 11.6 m crossing that
                // takes 10.7 s): who starts on green crosses in the clearance time after
                // it, until the cars get their green. Only the green alone was counted, so
                // nobody ever started on green and everybody went across on red after
                // 150 s, in front of moving cars.
                let window = pedestrian_window(ped.as_deref_mut(), net, t, next.lane, left);
                if (window as f64) < t_cross {
                    return Err("the green ends before they would be across");
                }
                return Ok(());
            }
        }
        let Some(ped) = ped else { return Ok(()) };
        // somebody who has waited long accepts a shorter gap (down to the time the crossing
        // takes, never less): a steady stream does not hold them for ever, but nobody walks
        // out in front of a car that is about to be there (after 45 s they used to ignore
        // the cars altogether)
        let margin = if held > 45.0 { 0.0 } else if held > 20.0 { 1.0 } else { 2.5 };
        for x in ped.crossings(net, next.lane) {
            for (p, v, half) in cars {
                let rel = *x - *p;
                let dist = rel.length();
                if dist > 90.0 {
                    continue;
                }
                if dist < half + 1.5 {
                    return Err("a vehicle stands on the crossing");
                }
                let speed = v.length();
                if speed < 0.5 {
                    continue;
                }
                let dir = *v / speed;
                let along = rel.dot(dir);
                let lateral = rel.perp_dot(dir).abs();
                if along > -half && lateral < 3.5 && along / speed < t_cross + margin {
                    return Err("waits for a car to pass");
                }
            }
        }
        Ok(())
    }

    /// Take over where the crowd moved person `i`.
    #[allow(clippy::too_many_arguments)]
    fn apply(
        &mut self,
        i: usize,
        w: &Walker,
        want: &Want,
        dt: f32,
        world: &World,
        net: Option<&Network>,
        buses: &[BusNow],
        bus_ix: &HashMap<BusId, usize>,
    ) {
        let dt64 = dt as f64;
        let time = self.time;
        let p = &mut self.people[i];
        let speed = w.vel.length();
        // somebody pressed against somebody else for seconds slips past them
        if want.vel.length() > 0.2 && speed < 0.08 {
            p.stuck += dt;
        } else if speed > 0.2 {
            p.stuck = 0.0;
        }
        p.detour = (p.detour - dt).max(0.0);
        if p.ghost > 0.0 {
            p.ghost -= dt;
        } else if p.stuck > 2.5 {
            p.ghost = 1.5;
            p.stuck = 0.0;
            if debug_pax() {
                log::info!(
                    "t={time:.1} pax {} ({}) is stuck and slips past",
                    p.label(),
                    p.state.name()
                );
            }
        }
        p.vel = w.vel;
        match p.place {
            Place::Ground => {
                p.position.x = w.pos.x;
                p.position.y = w.pos.y;
                if let Some(z) = world.walk_height_near(p.position.x, p.position.y, p.position.z) {
                    // up a kerb quickly, down it smoothly (the feet find the kerb themselves);
                    // more than a kerb below the surface is no step but a wrong height (the
                    // pavement's tile came after them): straight onto it
                    // (and whoever stands still simply stands on it: waiting people sank
                    // into a pavement that came after them and rose only when they walked)
                    p.position.z = if z - p.position.z > 0.35 || speed < 0.05 {
                        z
                    } else if z > p.position.z {
                        z.min(p.position.z + 1.5 * dt64)
                    } else {
                        z.max(p.position.z - 2.0 * dt64)
                    };
                }
            }
            Place::Bus(b, l) => {
                let here = Vec3::new(w.pos.x as f32, w.pos.y as f32, l.z);
                let z = l.z;
                let local = Vec3::new(here.x, here.y, z);
                p.place = Place::Bus(b, local);
                if let Some(bn) = bus_ix.get(&b).map(|k| &buses[*k]) {
                    p.position = bn.world(local);
                    p.interior = bn.interior;
                    p.tilt = bn.tilt_at(local);
                }
            }
        }
        // progress along the pavement
        if let Some(net) = net {
            let pos = p.position;
            match &mut p.state {
                State::Strolling(walk) => {
                    if let Some(leg) = walk.legs.get(walk.leg) {
                        walk.s = leg.project(net, pos, walk.s).max(walk.s - 0.3);
                    }
                }
                _ => {}
            }
        }
        let walking = if p.activity == Activity::Walk {
            speed > 0.12
        } else {
            speed > 0.3
        };
        let activity = if walking { Activity::Walk } else { want.idle };
        let inside = matches!(p.place, Place::Bus(..));
        let bus_heading = match p.place {
            Place::Bus(b, l) => bus_ix
                .get(&b)
                .map(|k| buses[*k].heading_at(l))
                .unwrap_or(0.0),
            Place::Ground => 0.0,
        };
        let current = if inside { p.lheading } else { p.heading };
        let target = if speed > 0.25 {
            Some(crowd::heading_of(w.vel))
        } else {
            want.face
        };
        // turning eases in and out (a constant rate started and stopped with a jerk): the
        // rate follows the angle still to go, up to the most a walker or a stander turns
        let turned = match target {
            Some(t) => {
                let left = crowd::angle_diff(current, t);
                let max_rate = if walking { 260.0 } else { 140.0 };
                let rate = (left.abs() * 5.0).min(max_rate).max(12.0);
                crowd::turn_towards(current, t, rate, dt64)
            }
            None => current,
        };
        if inside {
            p.lheading = turned;
            p.heading = bus_heading + turned;
        } else {
            p.heading = turned;
        }
        p.activity = activity;
    }

    /// People carried by a bus in their seat.
    fn carry(
        &mut self,
        i: usize,
        dt: f32,
        buses: &[BusNow],
        bus_ix: &HashMap<BusId, usize>,
        face: Option<f64>,
    ) {
        let p = &mut self.people[i];
        let Place::Bus(b, l) = p.place else { return };
        let Some(bn) = bus_ix.get(&b).map(|k| &buses[*k]) else {
            return;
        };
        p.position = bn.world(l);
        p.tilt = bn.tilt_at(l);
        p.vel = DVec2::ZERO;
        if let Some(f) = face {
            p.lheading = crowd::turn_towards(p.lheading, f, 150.0, dt as f64);
        }
        p.heading = bn.heading_at(l) + p.lheading;
        p.interior = bn.interior;
    }

    /// OMSI's `change_take`: the driver takes back the coins lying on the change tray.
    pub fn take_change_tray(&mut self) {
        if let Some(m) = self.money.as_mut() {
            m.clear(true);
        }
    }

    /// Coins the driver handed out (from the host's GiveChangeCoin list) onto the change point.
    pub fn give_change(
        &mut self,
        world: &World,
        renderer: &Renderer,
        scene: &mut Scene,
        coins: &[usize],
    ) {
        if coins.is_empty() {
            return;
        }
        let point = self.player_cabin.as_ref().and_then(|c| {
            c.data
                .change_points
                .first()
                .or(c.data.money_points.first())
                .cloned()
        });
        if let (Some(m), Some(pt)) = (self.money.as_mut(), point) {
            m.place(
                world,
                renderer,
                scene,
                coins,
                Vec3::from(pt.pos),
                pt.var,
                true,
            );
        }
    }

    pub fn sync_money(&mut self, renderer: &Renderer, scene: &mut Scene, bus: &VehicleInstance) {
        if let Some(m) = self.money.as_mut() {
            m.sync(renderer, scene, bus);
        }
    }

    /// Everybody's animation this frame (sub_626ae8): the passengers from what their task
    /// says (`PAX_State`, speed, the room height, the seat, the hand and the head), the
    /// pedestrians from their walk.
    fn animate(&mut self, dt: f32, world: &World, buses: &[BusNow], bus_ix: &HashMap<BusId, usize>) {
        let dt_ms = dt * 1000.0;
        for i in 0..self.people.len() {
            if let Some(pp) = self.people[i].puppet {
                if pp.mode == PuppetMode::Avatar {
                    self.animate_avatar(i, dt, world, buses, bus_ix);
                }
                continue;
            }
            let p = &self.people[i];
            let (input, footstep) = match &p.state {
                State::Pax(x) => {
                    let bn = x.inside.and_then(|b| bus_ix.get(&b).map(|k| &buses[*k]));
                    // a point of the bus in the person's own frame, Direct3D's axes
                    let own = |q: Vec3| -> Vec3 {
                        let v = q.as_dvec3() - x.pos;
                        let (s, c) = x.yaw.sin_cos();
                        let local = Vec3::new((v.x * c - v.y * s) as f32, (v.x * s + v.y * c) as f32, v.z as f32);
                        omsi_sim::human_omsi::d3d(local)
                    };
                    let reach = (x.reach && x.inside.is_some()).then(|| own(x.reach_at));
                    let look = match (x.look_driver, bn) {
                        (true, Some(b)) => b.cabin.data.driver_positions.first().map(|d| own(Vec3::from(d.pos) + Vec3::Z * 0.65)),
                        _ => None,
                    };
                    let kind = x.pax_state.round().clamp(0.0, 2.0) as u8;
                    let pack = match (x.step_pack, bn) {
                        (Some(k), Some(b)) => b.cabin.step_packs.get(k).cloned().map(|pk| (b.id, pk)),
                        _ => None,
                    };
                    (
                        AnimInput {
                            kind,
                            speed: x.speed,
                            moved: x.moved,
                            room_height: x.room,
                            seat_height: x.seat_h,
                            reach,
                            look,
                            smooth: x.smooth,
                            dt_ms,
                        },
                        pack,
                    )
                }
                _ => {
                    let v = p.vel.length() as f32;
                    (
                        AnimInput {
                            kind: if v > 0.05 { 1 } else { 0 },
                            speed: v,
                            moved: v * dt,
                            room_height: pax::OUTSIDE_ROOM,
                            dt_ms,
                            ..Default::default()
                        },
                        None,
                    )
                }
            };
            let p = &mut self.people[i];
            let ev = p.anim.advance(&p.ty.omsi, &input);
            // a foot down inside a vehicle: the link's step sound (outside there are none)
            if let (true, Some((bus, pack))) = (ev.step, footstep) {
                self.footfalls.push(ambience::Footfall { position: p.position, inside: true, own_bus: bus == BusId::Player, pack: Some(pack) });
            }
        }
    }

    /// Skin the people due for a new pose and push transforms to the renderer. Near people
    /// are posed every frame, far ones every few frames and people out of view rarely; the
    /// posing and skinning run in parallel.
    pub fn sync(&mut self, renderer: &Renderer, scene: &mut Scene, camera: DVec3) {
        for inst in self.hidden.drain(..) {
            renderer.set_params(scene, inst, &[], false, &[]);
        }
        let started = std::time::Instant::now();
        self.sync_frame = self.sync_frame.wrapping_add(1);
        let eye = self.eye;
        let from = eye.map(|e| e.pos).unwrap_or(camera);
        // synced only now and then (offscreen snapshots): everybody is posed afresh
        let all = self.time - self.last_sync > 0.12;
        let sdt = (self.time - self.last_sync).clamp(0.0, 0.5) as f32;
        self.last_sync = self.time;
        let mut due: Vec<bool> = Vec::with_capacity(self.people.len());
        for (k, p) in self.people.iter_mut().enumerate() {
            p.since_posed = p.since_posed.saturating_add(1);
            let d = p.position + DVec3::Z * 0.9 - from;
            let dist = d.length();
            let visible = match eye {
                Some(e) => dist < 4.0 || d.dot(e.fwd) / dist.max(1e-3) > e.cos_half - 0.15,
                None => true,
            };
            // everybody the eye can make out is posed every frame: a pose every other
            // frame at 12-30 m moved walkers in steps and made planted feet shiver
            // (within 30 m everybody, seen or not: the mirrors show the people behind the
            // bus, who were posed every twelfth frame and moved in jerks there)
            let every = if dist < 30.0 {
                1
            } else if !visible {
                12
            } else if dist < 45.0 {
                1
            } else if dist < 90.0 {
                2
            } else if dist < 160.0 {
                3
            } else {
                6
            };
            let every = if p.vel.length_squared() < 1e-4 && dist > 20.0 { every * 2 } else { every };
            // spread the far ones over the frames
            let turn = (self.sync_frame + k as u32) % every == 0;
            due.push(
                !p.skinned
                    || all
                    || (p.since_posed >= every && (turn || p.since_posed >= 2 * every)),
            );
        }
        let n_due = due.iter().filter(|d| **d).count();
        let pose_one = |p: &mut Person| {
            let Person { anim, ty, skins, skin_bones, pose_changed, .. } = p;
            *pose_changed = false;
            let bones = omsi_sim::human::slots_from_omsi(&anim.bones(&ty.omsi));
            if bones.iter().any(|b| !b.is_finite()) && !skins.is_empty() {
                // keep the last good mesh (the rest pose would be the file's T-pose)
                return;
            }
            // (the same bones as the mesh was made with: nothing to skin or upload)
            if skins.len() == ty.meshes.len() && skin_bones.as_ref().is_some_and(|b| b.iter().zip(&bones).all(|(a, c)| a.abs_diff_eq(*c, 1e-6))) {
                return;
            }
            skins.resize_with(ty.meshes.len(), Default::default);
            for (k, m) in ty.meshes.iter().enumerate() {
                let (pos, nrm) = &mut skins[k];
                skin(m, &bones, pos, nrm);
            }
            *skin_bones = Some(bones);
            *pose_changed = true;
        };
        // a handful is quicker on this thread than handed to the pool
        if n_due >= 8 {
            self.people
                .par_iter_mut()
                .zip(due.par_iter())
                .with_min_len(2)
                .filter(|(_, go)| **go)
                .for_each(|(p, _)| pose_one(p));
        } else {
            self.people
                .iter_mut()
                .zip(&due)
                .filter(|(_, go)| **go)
                .for_each(|(p, _)| pose_one(p));
        }
        let upload = std::time::Instant::now();
        for (p, &go) in self.people.iter_mut().zip(&due) {
            if go {
                if p.pose_changed || !p.skinned {
                    for (k, (id, _)) in p.meshes.iter().enumerate() {
                        if let Some((pos, nrm)) = p.skins.get(k) {
                            renderer.update_mesh(scene, *id, pos, nrm, &p.ty.meshes[k].data.uvs);
                        }
                    }
                }
                p.skinned = true;
                p.since_posed = 0;
                p.posed_at = (p.position, p.heading);
            }
            // riders go with their bus; on the ground a mesh not posed this frame goes on
            // with the body too (left where it was posed, a far walker moved in jerks -
            // its feet slide a few centimetres instead, which nobody sees at that distance)
            let (at, heading) = match (p.puppet, p.place) {
                (_, Place::Ground) if go => p.posed_at,
                _ => (p.position, p.heading),
            };
            // (riders with the tilt of their floor)
            let tilt = if matches!(p.place, Place::Bus(..)) { p.tilt } else { Mat4::IDENTITY };
            let xf = tilt * Mat4::from_rotation_z((-heading).to_radians() as f32);
            let lit_to = if matches!(p.place, Place::Bus(..)) { p.interior } else { 0.0 };
            p.lit += (lit_to - p.lit) * (sdt / 0.4).min(1.0);
            for (_, inst) in &p.meshes {
                renderer.set_transform(scene, *inst, at, xf);
                renderer.set_interior(scene, *inst, p.lit * 0.5);
            }
            if self.avatar_hidden.contains_key(&p.id) && omsi_cfg::env::var_os("OMSI_DEBUG_FOOT").is_some() && self.sync_frame % 30 == 0 {
                log::info!("avatar drawn at ({:.2}, {:.2}, {:.2}) heading {:.0} place {:?} go {}", at.x, at.y, at.z, heading, matches!(p.place, Place::Ground), go);
            }
            if let Some(hide) = self.avatar_hidden.get_mut(&p.id) {
                // (the first-person view: the avatar's own body out of the picture; set
                // every frame, the posing would show it again)
                for (_, inst) in &p.meshes {
                    renderer.set_params(scene, *inst, &[], !*hide, &[]);
                }
            }
            if let Some(t) = self.trace.as_mut() {
                // OMSI_TRACE_PAX: where the mesh is drawn and where its ankles are, per frame
                if (at - from).length() < 40.0 {
                    use std::io::Write;
                    let a = |k: usize| at + (xf.transform_vector3(p.ankles[k])).as_dvec3();
                    let (l, r) = (a(0), a(1));
                    let _ = writeln!(
                        t,
                        "{:.4},{},{},{},{},{:.4},{:.4},{:.4},{:.2},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.3},{:.3}",
                        self.time,
                        p.id,
                        p.state.name(),
                        matches!(p.place, Place::Ground) as u8,
                        go as u8,
                        at.x,
                        at.y,
                        at.z,
                        heading,
                        l.x,
                        l.y,
                        l.z,
                        r.x,
                        r.y,
                        r.z,
                        p.vel.x,
                        p.vel.y
                    );
                }
            }
        }
        // OMSI_CHECK_TPOSE=1: everybody drawn with the arms out (the file's rest pose): the
        // skinned mesh wider than 1.3 m from hand to hand
        if omsi_cfg::env::var_os("OMSI_CHECK_TPOSE").is_some() {
            for p in &self.people {
                let Some((pos, _)) = p.skins.first() else {
                    log::info!("t-pose? {} {}: never skinned", p.label(), p.state_name());
                    continue;
                };
                let (lo, hi) = pos.iter().fold((f32::MAX, f32::MIN), |(lo, hi), v| (lo.min(v.x), hi.max(v.x)));
                if hi - lo > 1.3 {
                    let pax = match &p.state {
                        State::Pax(x) => format!("pax_state {} speed {:.2} seat_h {:.2} room {:.2} st {}", x.pax_state, x.speed, x.seat_h, x.room, x.st),
                        _ => String::new(),
                    };
                    log::info!("t-pose: {} {} width {:.2} skinned {} {pax}", p.label(), p.state_name(), hi - lo, p.skinned);
                }
            }
        }
        self.pose_stats.0 += 1;
        self.pose_stats.1 += n_due;
        self.pose_stats.2 += started.elapsed().as_secs_f64() * 1000.0;
        self.pose_stats.3 += upload.elapsed().as_secs_f64() * 1000.0;
    }


}


#[derive(Debug, Clone, Copy, PartialEq)]
enum PuppetMode {
    /// The player on foot (or another player's walker): moved by the game, see `avatar`.
    Avatar,
}

/// A person the game moves itself (the player on foot).
#[derive(Debug, Clone, Copy)]
struct Puppet {
    mode: PuppetMode,
}

// ---------------------------------------------------------------------------------------
// Avatars: the player got up from the seat (`on_foot`), or another player walks about. The
// game moves them; the people's animation poses them - the gait and its feet on the
// ground, sitting down on a seat and getting up - so every change is eased, never a jump.

/// What the game wants of an avatar this frame.
#[derive(Debug, Clone, Copy)]
pub struct AvatarCmd {
    /// The feet (on foot), in the world.
    pub pos: DVec3,
    /// Facing (degrees, OMSI's).
    pub heading: f64,
    /// Velocity over the ground (m/s).
    pub vel: DVec2,
    /// How high the feet are over the ground (a jump).
    pub lift: f64,
    /// Sitting on this seat of this bus.
    pub seat: Option<(BusId, usize)>,
    /// Standing on a vehicle's floor at this height rather than on the ground (walking
    /// inside a bus: the feet stay on its floor, not reaching down to the road).
    pub floor: Option<f64>,
    /// Standing or walking inside this bus at this point of its cabin (bus frame): placed
    /// in the bus's frame as it is this frame, as its passengers are (a world point taken
    /// a frame earlier left the figure trembling behind the moving bus).
    pub aboard: Option<(BusId, Vec3)>,
}

/// A seat an avatar may take: which, in which bus.
#[derive(Debug, Clone, Copy)]
pub struct SeatSpot {
    pub bus: BusId,
    pub seat: usize,
}

impl Humans {
    /// Put avatar `key` where `cmd` says (made on its first call, of figure `kind`).
    pub fn avatar(&mut self, key: u32, world: &World, renderer: &Renderer, scene: &mut Scene, cmd: AvatarCmd, kind: u64) {
        let known = self.avatars.get(&key).copied().filter(|id| self.people.iter().any(|p| p.id == *id));
        if known.is_none() {
            let state = State::Idle;
            let n = self.types.len().max(1) as u64;
            let Some(i) = self.spawn_as(world, renderer, scene, cmd.pos, cmd.heading, state, Some((kind % n) as usize)) else { return };
            self.people[i].puppet = Some(Puppet { mode: PuppetMode::Avatar });
            self.avatars.insert(key, self.people[i].id);
        }
        // a seat taken is kept from the passengers; one left is theirs again
        let before = self.avatar_cmds.get(&key).and_then(|c| c.seat);
        if before != cmd.seat {
            if let Some((b, k)) = before {
                self.free_seat(b, k);
            }
            if let Some((b, k)) = cmd.seat {
                if let Some(t) = self.seats.get_mut(&b).and_then(|v| v.get_mut(k)) {
                    *t = true;
                }
            }
        }
        self.avatar_cmds.insert(key, cmd);
    }

    /// Take avatar `key` away.
    pub fn avatar_remove(&mut self, key: u32) {
        if let Some(c) = self.avatar_cmds.remove(&key) {
            if let Some((b, k)) = c.seat {
                self.free_seat(b, k);
            }
        }
        if let Some(id) = self.avatars.remove(&key) {
            if let Some(i) = self.people.iter().position(|p| p.id == id) {
                let p = self.people.swap_remove(i);
                self.retire(&p);
            }
        }
    }

    /// Draw avatar `key` or not (the first-person view looks out of its eyes).
    pub fn avatar_show(&mut self, key: u32, show: bool) {
        if let Some(id) = self.avatars.get(&key) {
            self.avatar_hidden.insert(*id, !show);
        }
    }

    /// Where avatar `key` is drawn: its feet, facing, and its eyes.
    pub fn avatar_body(&self, key: u32) -> Option<(DVec3, f64, DVec3)> {
        let id = self.avatars.get(&key)?;
        let p = self.people.iter().find(|p| p.id == *id)?;
        let rig = &p.ty.rig;
        let eye_h = (rig.head_top - 0.11 * rig.scale) as f64;
        let eye = match (p.place, self.avatar_cmds.get(&key).and_then(|c| c.seat)) {
            (Place::Bus(b, _), Some((_, k))) => {
                let bn = self.last_buses.iter().find(|x| x.id == b)?;
                let s = bn.cabin.seats.get(k)?;
                // sitting: the eyes over the hip, a little back
                let r = s.rot.to_radians();
                bn.world(s.pos + Vec3::new(-r.sin() * 0.05, -r.cos() * 0.05, (eye_h - rig.hip[0].z as f64) as f32 + 0.04))
            }
            _ => p.position + DVec3::new(0.0, 0.0, eye_h),
        };
        Some((p.position, p.heading, eye))
    }

    /// The seat nearest `at` with a door of its bus within `reach` of it (people and
    /// the other avatars' seats taken), among the buses of the last tick; `only` limits it
    /// to one bus.
    pub fn seat_near(&self, at: DVec3, reach: f64, only: Option<BusId>) -> Option<SeatSpot> {
        let mut best: Option<(f64, SeatSpot)> = None;
        for bn in &self.last_buses {
            if only.map(|o| o != bn.id).unwrap_or(false) {
                continue;
            }
            // the nearest door (entries and exits: any door will do to get in)
            let door = bn
                .cabin
                .entries
                .iter()
                .chain(bn.cabin.exits.iter())
                .map(|d| bn.world(d.outside))
                .min_by(|a, b| (*a - at).length().total_cmp(&(*b - at).length()));
            let Some(door) = door else { continue };
            let d = (door - at).truncate().length();
            if d > reach {
                continue;
            }
            let taken = self.seats.get(&bn.id);
            let seat = bn
                .cabin
                .seats
                .iter()
                .enumerate()
                .filter(|(k, s)| s.seated && !taken.and_then(|t| t.get(*k)).copied().unwrap_or(false))
                .min_by(|a, b| (bn.world(a.1.floor) - door).length().total_cmp(&(bn.world(b.1.floor) - door).length()))
                .map(|(k, _)| k);
            let Some(seat) = seat else { continue };
            if best.map(|b| d < b.0).unwrap_or(true) {
                best = Some((d, SeatSpot { bus: bn.id, seat }));
            }
        }
        best.map(|b| b.1)
    }

    /// Where the doors of a bus are now (outside, in the world).
    pub fn bus_doors(&self, bus: BusId) -> Vec<DVec3> {
        self.last_buses
            .iter()
            .find(|b| b.id == bus)
            .map(|bn| bn.cabin.entries.iter().chain(bn.cabin.exits.iter()).map(|d| bn.world(d.outside)).collect())
            .unwrap_or_default()
    }

    /// The door of vehicle `v` nearest its driver's seat (outside, in the world): where the
    /// driver gets in and out.
    pub fn vehicle_driver_door(&mut self, v: &VehicleInstance) -> Option<DVec3> {
        // (a van's own cab door first: its driver does not climb in through the sliding door)
        if let Some(d) = self.vehicle_cab_door(v) {
            return Some(d);
        }
        let cabin = self.cabin_for(v)?;
        let seat = cabin.data.driver_positions.first().map(|d| Vec3::from(d.pos)).unwrap_or(Vec3::new(-0.8, 4.5, 1.0));
        let door = cabin.entries.iter().chain(cabin.exits.iter()).min_by(|a, b| (a.outside - seat).truncate().length().total_cmp(&(b.outside - seat).truncate().length()))?;
        let trailers = part_frames(v, &cabin);
        Some(train_point(v.position, &v.body_rotation(), &trailers, door.outside))
    }

    /// A door of the driver's own beside the driver's seat (a van's or a coach's cab door:
    /// on the driver's side, level with the seat), in the world, outside.
    pub fn vehicle_cab_door(&mut self, v: &VehicleInstance) -> Option<DVec3> {
        let cabin = self.cabin_for(v)?;
        let seat = cabin.data.driver_positions.first().map(|d| Vec3::from(d.pos))?;
        let door = cabin
            .entries
            .iter()
            .chain(cabin.exits.iter())
            .filter(|d| d.outside.x * seat.x > 0.0 && (d.outside.y - seat.y).abs() < 1.5)
            .min_by(|a, b| (a.outside - seat).truncate().length().total_cmp(&(b.outside - seat).truncate().length()))
            .map(|d| d.outside);
        // a van or minibus (the W906: its cabin knows only the sliding door, the passengers'):
        // the driver's door beside the seat, which every such vehicle has
        let door = door.or_else(|| {
            let bb = v.ty.def.bounding_box?;
            (bb[1] < 8.5 && seat.x.abs() > 0.2).then(|| Vec3::new(seat.x.signum() * (bb[0] * 0.5 + bb[3] * seat.x.signum() + 0.45), seat.y, 0.0))
        })?;
        let trailers = part_frames(v, &cabin);
        Some(train_point(v.position, &v.body_rotation(), &trailers, door))
    }

    /// Put `ty` among the figures (once) and give its index: the player's own figure.
    pub fn type_index(&mut self, ty: Arc<HumanType>) -> usize {
        if let Some(i) = self.types.iter().position(|t| Arc::ptr_eq(t, &ty) || t.def.path == ty.def.path) {
            return i;
        }
        self.types.push(ty);
        self.types.len() - 1
    }

    /// Where the doors of vehicle `v` are now (outside, in the world), entries first.
    pub fn vehicle_doors(&mut self, v: &VehicleInstance) -> Vec<DVec3> {
        let Some(cabin) = self.cabin_for(v) else { return Vec::new() };
        let trailers = part_frames(v, &cabin);
        let rot = v.body_rotation();
        cabin.entries.iter().chain(cabin.exits.iter()).map(|d| train_point(v.position, &rot, &trailers, d.outside)).collect()
    }

    /// A walker inside bus `bus` moving from cabin point `local` by `step` (bus frame,
    /// metres): kept within a corridor round the cabin's own path network (the aisles,
    /// the door areas, the space by the driver) and on its floor. Gives the new cabin point
    /// and where that is in the world now.
    pub fn cabin_walk(&self, bus: BusId, local: Vec3, step: glam::Vec2) -> Option<(Vec3, DVec3)> {
        const WIDTH: f32 = 0.3;
        let bn = self.last_buses.iter().find(|b| b.id == bus)?;
        let pts = &bn.cabin.graph.points;
        let want = glam::Vec2::new(local.x + step.x, local.y + step.y);
        let mut best: Option<(f32, glam::Vec2, f32)> = None;
        for &(a, b, _) in &bn.cabin.links {
            let (Some(pa), Some(pb)) = (pts.get(a.max(0) as usize), pts.get(b.max(0) as usize)) else { continue };
            let (a2, b2) = (pa.truncate(), pb.truncate());
            let ab = b2 - a2;
            let t = if ab.length_squared() > 1e-6 { ((want - a2).dot(ab) / ab.length_squared()).clamp(0.0, 1.0) } else { 0.0 };
            let q = a2 + ab * t;
            let d = (want - q).length() + (local.z - (pa.z + (pb.z - pa.z) * t)).abs();
            if best.map(|x| d < x.0).unwrap_or(true) {
                best = Some((d, q, pa.z + (pb.z - pa.z) * t));
            }
        }
        if best.is_none() {
            for pt in pts {
                let d = (want - pt.truncate()).length();
                if best.map(|x| d < x.0).unwrap_or(true) {
                    best = Some((d, pt.truncate(), pt.z));
                }
            }
        }
        let (d, q, z) = best?;
        let xy = if d > WIDTH { q + (want - q) / d * WIDTH } else { want };
        // not through the seats and the driver's place: no nearer to one than 0.38 m
        // (walking away from one that close is let be)
        let from = local.truncate();
        let solid = bn.cabin.seats.iter().filter(|s| s.seated).map(|s| s.pos.truncate()).chain(bn.cabin.data.driver_positions.iter().map(|d| glam::Vec2::new(d.pos[0], d.pos[1])));
        for c in solid {
            let (dn, d0) = ((xy - c).length(), (from - c).length());
            if dn < 0.38 && dn < d0 {
                return Some((local, bn.world(local)));
            }
        }
        let l = Vec3::new(xy.x, xy.y, z);
        Some((l, bn.world(l)))
    }

    /// The doors of bus `bus`: the threshold in the cabin, where one stands outside (world),
    /// which side of the bus (+1 right) and whether it is open now.
    pub fn cabin_doors(&self, bus: BusId) -> Vec<(Vec3, DVec3, f32, bool)> {
        let Some(bn) = self.last_buses.iter().find(|b| b.id == bus) else { return Vec::new() };
        let (eo, xo) = bn.walk_open.as_ref().map(|w| (&w.0, &w.1)).unwrap_or((&bn.entry_open, &bn.exit_open));
        let entries = bn.cabin.entries.iter().enumerate().map(|(k, d)| (d, eo.get(k).copied().unwrap_or(false)));
        let exits = bn.cabin.exits.iter().enumerate().map(|(k, d)| (d, xo.get(k).copied().unwrap_or(false)));
        entries.chain(exits).map(|(d, open)| (d.inside, bn.world(d.outside), d.side, open)).collect()
    }

    /// The buses of the last tick within `r` of `at`, the own first.
    pub fn bus_ids_near(&self, at: DVec3, r: f64) -> Vec<BusId> {
        let mut v: Vec<(BusId, f64)> = self.last_buses.iter().map(|b| (b.id, (b.pos - at).truncate().length())).filter(|x| x.1 < r).collect();
        v.sort_by(|a, b| (a.0 != BusId::Player).cmp(&(b.0 != BusId::Player)).then(a.1.total_cmp(&b.1)));
        v.into_iter().map(|x| x.0).collect()
    }

    /// Where the cabin point `local` of bus `bus` is in the world now, and the bus's heading.
    pub fn cabin_world(&self, bus: BusId, local: Vec3) -> Option<(DVec3, f64)> {
        let bn = self.last_buses.iter().find(|b| b.id == bus)?;
        Some((bn.world(local), bn.heading))
    }

    /// The free seat of bus `bus` nearest the world point `at` (for a walker inside it).
    pub fn seat_nearest(&self, bus: BusId, at: DVec3, reach: f64) -> Option<usize> {
        let bn = self.last_buses.iter().find(|b| b.id == bus)?;
        let taken = self.seats.get(&bn.id);
        bn.cabin
            .seats
            .iter()
            .enumerate()
            .filter(|(k, s)| s.seated && !taken.and_then(|t| t.get(*k)).copied().unwrap_or(false))
            .map(|(k, s)| (k, (bn.world(s.floor) - at).truncate().length()))
            .filter(|(_, d)| *d < reach)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|x| x.0)
    }

    /// The cabin path point nearest seat `seat` of bus `bus` (where one stands up to).
    pub fn seat_stand(&self, bus: BusId, seat: usize) -> Option<Vec3> {
        let bn = self.last_buses.iter().find(|b| b.id == bus)?;
        let s = bn.cabin.seats.get(seat)?;
        // (on the seat's own deck: in a double-decker the nearest point in plan could be
        // the one straight above or below it)
        let d = |a: &Vec3| (a.truncate() - s.floor.truncate()).length() + (a.z - s.floor.z).abs() * 3.0;
        bn.cabin.graph.points.iter().copied().min_by(|a, b| d(a).total_cmp(&d(b)))
    }

    /// Cabin point `local` of vehicle `v` in the world (before the buses' first tick).
    pub fn vehicle_cabin_world(&mut self, v: &VehicleInstance, local: Vec3) -> Option<DVec3> {
        let cabin = self.cabin_for(v)?;
        let trailers = part_frames(v, &cabin);
        Some(train_point(v.position, &v.body_rotation(), &trailers, local))
    }

    /// Where the driver stands up in vehicle `v`'s cabin: the cabin's path point nearest the
    /// driver's seat (bus frame).
    pub fn driver_stand(&mut self, v: &VehicleInstance) -> Option<Vec3> {
        let cabin = self.cabin_for(v)?;
        let seat = cabin.data.driver_positions.first().map(|d| Vec3::from(d.pos)).unwrap_or(Vec3::new(-0.8, 4.5, 1.0));
        // the driver's position is the hip, half a metre over the cab floor: a double
        // decker's upper deck lies straight over the cab and was as near in plan, and the
        // driver who got up stood in the roof over the windscreen
        let d = |a: &Vec3| (a.truncate() - seat.truncate()).length() + (a.z - (seat.z - 0.5)).abs() * 3.0;
        cabin.graph.points.iter().copied().min_by(|a, b| d(a).total_cmp(&d(b)))
    }

    /// How many people are in (or boarding, riding, leaving) bus `bus`.
    pub fn people_in(&self, bus: BusId) -> usize {
        self.people.iter().filter(|p| matches!(p.place, Place::Bus(b, _) if b == bus) || p.state.bus() == Some(bus)).count()
    }

    /// Where bus `bus` stands (its origin), as of the last tick.
    pub fn bus_center(&self, bus: BusId) -> Option<DVec3> {
        self.last_buses.iter().find(|b| b.id == bus).map(|b| b.pos)
    }

    /// Is `bus` among the buses of the last tick?
    pub fn bus_here(&self, bus: BusId) -> bool {
        self.last_buses.iter().any(|b| b.id == bus)
    }

    /// The player's (or another player's) body on foot, animated as Omsi.exe animates its
    /// people: sitting on a seat (its hip on the `[passpos]`), walking or standing.
    fn animate_avatar(&mut self, i: usize, dt: f32, world: &World, buses: &[BusNow], bus_ix: &HashMap<BusId, usize>) {
        let id = self.people[i].id;
        let Some(key) = self.avatars.iter().find(|(_, v)| **v == id).map(|(k, _)| *k) else { return };
        let Some(cmd) = self.avatar_cmds.get(&key).copied() else { return };
        let dt_ms = dt * 1000.0;
        let seated = cmd.seat.and_then(|(b, k)| {
            let bn = bus_ix.get(&b).map(|x| &buses[*x])?;
            let s = bn.cabin.seats.get(k)?.clone();
            Some((b, s, bn))
        });
        let seatheight = self.people[i].ty.def.seat_height;
        let p = &mut self.people[i];
        let input = match seated {
            Some((b, s, bn)) => {
                // on the seat, in its bus's frame (set_task(7): the feet the human's seat
                // height under the seat point, facing the way the seat does)
                let l = if s.seated { s.pos - Vec3::Z * seatheight } else { s.pos };
                p.place = Place::Bus(b, l);
                p.lheading = s.rot as f64;
                p.position = bn.world(l);
                p.tilt = bn.tilt_at(l);
                p.heading = bn.heading_at(l) + p.lheading;
                p.interior = bn.interior;
                p.vel = DVec2::ZERO;
                p.activity = if s.seated { Activity::Sit } else { Activity::Stand };
                AnimInput { kind: if s.seated { 2 } else { 0 }, seat_height: s.height, room_height: pax::OUTSIDE_ROOM, dt_ms, ..Default::default() }
            }
            None if cmd.aboard.is_some_and(|(b, _)| bus_ix.contains_key(&b)) => {
                let (b, l) = cmd.aboard.unwrap();
                let bn = &buses[bus_ix[&b]];
                let bh = bn.heading_at(l);
                p.place = Place::Bus(b, l);
                p.lheading = wrap_heading(cmd.heading - bh);
                p.position = bn.world(l);
                p.tilt = bn.tilt_at(l);
                p.heading = cmd.heading;
                p.interior = bn.interior;
                p.vel = cmd.vel;
                let v = cmd.vel.length() as f32;
                p.activity = if v > 0.05 { Activity::Walk } else { Activity::Stand };
                AnimInput { kind: (v > 0.05) as u8, speed: v, moved: v * dt, room_height: pax::OUTSIDE_ROOM, dt_ms, ..Default::default() }
            }
            None => {
                p.place = Place::Ground;
                p.tilt = Mat4::IDENTITY;
                p.interior = 0.0;
                let ground = cmd.floor.unwrap_or_else(|| world.walk_height(cmd.pos.x, cmd.pos.y).unwrap_or(cmd.pos.z));
                let origin = DVec3::new(cmd.pos.x, cmd.pos.y, if cmd.floor.is_some() { ground } else { cmd.pos.z.max(ground) } + cmd.lift.max(0.0));
                p.position = origin;
                p.heading = cmd.heading;
                p.vel = cmd.vel;
                let v = cmd.vel.length() as f32;
                p.activity = if v > 0.05 { Activity::Walk } else { Activity::Stand };
                AnimInput { kind: (v > 0.05) as u8, speed: v, moved: v * dt, room_height: pax::OUTSIDE_ROOM, dt_ms, ..Default::default() }
            }
        };
        p.anim.advance(&p.ty.omsi, &input);
    }
}

// ---------------------------------------------------------------------------------------
// LAN play (see `lan_world`): a host keeps people around every player, tells the clients
// where they are and hands the waiting ones over to a client's bus; a client draws the
// host's people instead of its own and simulates only those who board its bus.

/// Where one of the host's people is this frame, as a client draws them.
#[derive(Debug, Clone, Copy)]
pub struct MirrorPose {
    pub pos: DVec3,
    pub heading: f64,
    pub vel: DVec2,
    pub activity: Activity,
    /// Aboard a timetable bus: (its id, the point of its frame, heading in its frame, the
    /// seat or standing place, if known).
    pub aboard: Option<(u64, Vec3, f64, Option<usize>)>,
    /// Waiting at a stop: (the stop object, the waiting place).
    pub waiting: Option<(i64, usize)>,
}

/// The bus id (`BusId::Ai`) another LAN player's bus has among the buses here: far above
/// the traffic's car ids.
pub fn remote_bus_id(player: u32) -> u64 {
    (1 << 40) | player as u64
}

/// The bus id (`BusId::Ai`) of a vehicle the player placed (`Player::uid`).
pub fn placed_bus_id(uid: u64) -> u64 {
    (2 << 40) | uid
}

/// The player whose bus `remote_bus_id` gave this id (None for a traffic bus).
pub fn remote_bus_player(bus: u64) -> Option<u32> {
    (bus >> 40 == 1).then_some((bus & 0xFFFF_FFFF) as u32)
}

/// One of the host's people as it tells the clients.
pub struct LanPerson {
    pub id: u32,
    pub ty: Arc<HumanType>,
    pub pos: DVec3,
    pub heading: f64,
    pub speed: f64,
    pub activity: Activity,
    pub aboard: Option<(u64, Vec3, f64, Option<usize>)>,
    pub waiting: Option<(i64, usize)>,
}

impl Humans {
    /// Is `p` further than `r` from us and from every other LAN player?
    fn far_from_players(&self, p: DVec3, r: f64) -> bool {
        (p - self.center).length() > r && self.lan_centers.iter().all(|c| (p - *c).length() > r)
    }

    /// The stops and pavements around the other players of a LAN session (host).
    fn populate_lan_centers(
        &mut self,
        world: &World,
        net: &Network,
        renderer: &Renderer,
        scene: &mut Scene,
    ) {
        if self.lan_centers.is_empty() {
            return;
        }
        let mine = self.center;
        for c in self.lan_centers.clone() {
            if (c - mine).length() < 150.0 {
                continue;
            }
            self.populate_with(world, Some(net), renderer, scene, c);
            self.populate_on_foot(world, net, renderer, scene, 1.0);
        }
        self.center = mine;
    }

    /// Everybody within `radius` of `near` the clients may see (host): on foot, waiting at
    /// a stop, or aboard a timetable bus - not the riders of our own bus, which the others
    /// see from outside only.
    pub fn lan_people(&self, near: DVec3, radius: f64) -> Vec<LanPerson> {
        let r2 = radius * radius;
        self.people
            .iter()
            .filter(|p| p.puppet.is_none() && !p.remote)
            .filter(|p| (p.position - near).length_squared() < r2)
            .filter_map(|p| {
                let aboard = match p.place {
                    Place::Bus(BusId::Player, _) => return None,
                    Place::Bus(BusId::Ai(bus), l) => Some((
                        bus,
                        l,
                        p.lheading,
                        match &p.state {
                            State::Pax(x) if x.task == Task::SittingInBus => x.seat,
                            _ => None,
                        },
                    )),
                    Place::Ground => None,
                };
                let waiting = match &p.state {
                    State::Pax(x) if aboard.is_none() && x.task == Task::WaitingForBus => x.stop.zip(x.spot),
                    _ => None,
                };
                Some(LanPerson {
                    id: p.id,
                    ty: p.ty.clone(),
                    pos: p.position,
                    heading: p.heading,
                    speed: if aboard.is_some() { 0.0 } else { p.vel.length() },
                    activity: p.activity,
                    aboard,
                    waiting,
                })
            })
            .collect()
    }

    /// A client's bus takes these waiting people (host): those still waiting leave our
    /// world (they are the client's now); returns them. Somebody who has meanwhile walked
    /// up to another bus stays ours.
    pub fn hand_over(&mut self, player: u32, ids: &[u32]) -> Vec<u32> {
        let mut out = Vec::new();
        for id in ids {
            let Some(i) = self.people.iter().position(|p| p.id == *id) else {
                continue;
            };
            if !matches!(&self.people[i].state, State::Pax(x) if x.task == Task::WaitingForBus) || self.people[i].remote {
                continue;
            }
            // (still counted at their stop while that bus stands there, as the people
            // boarding a bus of ours are: see `handed`)
            if let Some(stop) = self.pax(i).and_then(|x| x.stop) {
                self.handed.push((stop, remote_bus_id(player)));
            }
            self.release(i);
            let p = self.people.swap_remove(i);
            self.retire(&p);
            out.push(*id);
        }
        out
    }

    /// Draw the host's people from now on (`on`), or simulate our own again. Everybody who
    /// is not getting on, riding or getting off our bus goes (the host's come instead; the
    /// host's copies cannot walk on by themselves).
    pub fn set_mirror(&mut self, on: bool) {
        if self.mirror == on {
            return;
        }
        self.mirror = on;
        let keep = |p: &Person| !p.remote && p.state.bus() == Some(BusId::Player);
        let mut i = 0;
        while i < self.people.len() {
            if keep(&self.people[i]) || self.people[i].puppet.is_some() {
                i += 1;
                continue;
            }
            self.release(i);
            let p = self.people.swap_remove(i);
            self.retire(&p);
        }
        // our own people from now on are numbered far above the host's (those riding with
        // us already too)
        if on {
            self.next_id = self.next_id.max(1 << 30);
            for p in self.people.iter_mut().filter(|p| p.puppet.is_none()) {
                p.id = self.next_id;
                self.next_id += 1;
            }
        }
        for s in self.stops.values_mut() {
            for t in s.taken.iter_mut() {
                *t = false;
            }
        }
        self.claims_out.clear();
        self.claimed.clear();
        self.mirror_wait.clear();
    }

    /// The human type of a file relative to a content root (`Humans/…/x.hum`).
    pub fn type_by_file(&self, file: &str) -> Option<usize> {
        let want = file.replace('\\', "/").to_ascii_lowercase();
        self.types.iter().position(|t| {
            t.def
                .path
                .to_string_lossy()
                .replace('\\', "/")
                .to_ascii_lowercase()
                .ends_with(&want)
        })
    }

    /// The file of a human type relative to its content root (`Humans/…/x.hum`).
    pub fn type_file(ty: &HumanType) -> String {
        let p = ty.def.path.to_string_lossy().replace('\\', "/");
        match p.to_ascii_lowercase().rfind("/humans/") {
            Some(k) => p[k + 1..].to_string(),
            None => p,
        }
    }

    /// One of the host's people appears here (client).
    pub fn mirror_add(
        &mut self,
        world: &World,
        renderer: &Renderer,
        scene: &mut Scene,
        id: u32,
        ty: usize,
        pose: &MirrorPose,
    ) -> bool {
        if self.people.iter().any(|p| p.id == id) {
            return false;
        }
        let state = State::Idle;
        let Some(i) =
            self.spawn_as(world, renderer, scene, pose.pos, pose.heading, state, Some(ty))
        else {
            return false;
        };
        self.next_id -= 1;
        let p = &mut self.people[i];
        p.id = id;
        p.anim = OmsiAnim::default();
        p.remote = true;
        self.mirror_set(id, pose);
        true
    }

    /// Where one of the host's people is this frame (client).
    pub fn mirror_set(&mut self, id: u32, pose: &MirrorPose) {
        let Some(p) = self.people.iter_mut().find(|p| p.id == id && p.remote) else {
            return;
        };
        p.position = pose.pos;
        p.heading = pose.heading;
        p.vel = pose.vel;
        p.activity = pose.activity;
        match pose.waiting {
            Some(w) => {
                self.mirror_wait.insert(id, w);
            }
            None => {
                self.mirror_wait.remove(&id);
            }
        }
        match pose.aboard {
            Some((bus, local, lheading, _)) => {
                p.place = Place::Bus(BusId::Ai(bus), local);
                p.lheading = lheading;
                p.vel = DVec2::ZERO;
            }
            None => p.place = Place::Ground,
        }
    }

    /// One of the host's people has gone (client).
    pub fn mirror_remove(&mut self, id: u32) {
        if let Some(i) = self.people.iter().position(|p| p.id == id && p.remote) {
            let p = self.people.swap_remove(i);
            self.retire(&p);
        }
        self.claimed.remove(&id);
        self.mirror_wait.remove(&id);
    }

    /// A remote person this frame: they stand where the host put them.
    fn mirror_want(&mut self, i: usize, _buses: &[BusNow]) -> Want {
        Want::stand(None, self.people[i].activity)
    }

    /// The type of one of our people (host).
    pub fn lan_people_by_id(&self, id: u32) -> Option<Arc<HumanType>> {
        self.people
            .iter()
            .find(|p| p.id == id && !p.remote)
            .map(|p| p.ty.clone())
    }

    /// Where the host's people are drawn (client; `OMSI_LAN_TRACE`).
    pub fn mirror_positions(&self) -> Vec<(u32, DVec3)> {
        self.people
            .iter()
            .filter(|p| p.remote && p.place == Place::Ground)
            .map(|p| (p.id, p.position))
            .collect()
    }

    /// Waiting people to ask the host for (client).
    pub fn take_claims(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.claims_out)
    }

    /// The host handed this waiting person over to our bus (client). (The people of a LAN
    /// game stay the host's: nothing is taken over.)
    pub fn grant(&mut self, id: u32) -> bool {
        self.claimed.remove(&id);
        let Some((stop, spot)) = self.mirror_wait.remove(&id) else { return false };
        let Some(i) = self.people.iter().position(|p| p.id == id && p.remote) else { return false };
        if !self.stops.contains_key(&stop) {
            return false;
        }
        // ours from now on: waiting at that place, for the bus that stands there (the first
        // listed, as Omsi.exe takes it without a line record)
        let sp = self.stops[&stop].spots.get(spot).cloned();
        let seatheight = self.people[i].ty.def.seat_height;
        let walk = 1.1 + (self.rand_f() as f32 * 2.0 - 1.0) * 0.2;
        let mut pax = Pax::new(walk);
        pax.task = Task::WaitingForBus;
        pax.stop = Some(stop);
        // what a waiting person of ours has (sub_626044 and task 6): a destination drawn
        // from the stop (both sides load the same map) and a distance to ride without one;
        // with neither they would get off again at once (#813)
        let (dest, line) = self.draw_dest(stop);
        pax.dest = dest;
        pax.line = line;
        pax.ride_km = self.rand_f() as f32 * 19.0 + 1.0;
        pax.pos = self.people[i].position;
        pax.yaw = self.people[i].heading.to_radians();
        if let Some(sp) = sp {
            pax.spot = Some(spot);
            if let Some(t) = self.stops.get_mut(&stop).unwrap().taken.get_mut(spot) {
                *t = true;
            }
            if sp.height != 0.0 {
                pax.seat_h = sp.height;
                pax.pos = sp.pos - DVec3::Z * seatheight as f64;
                pax.pax_state = 2.0;
            }
            pax.yaw = sp.face.to_radians();
        }
        let p = &mut self.people[i];
        p.remote = false;
        p.state = State::Pax(Box::new(pax));
        true
    }

    /// The host's people waiting at the stop our bus is listed at (client): ask for them.
    fn claim_waiting(&mut self) {
        if !self.mirror {
            return;
        }
        let now = self.time;
        self.claimed.retain(|_, t| now - *t < 10.0);
        let at: Vec<i64> = self
            .stops
            .iter()
            .filter(|(_, s)| s.buses.iter().any(|b| b.0 == BusId::Player))
            .map(|(id, _)| *id)
            .collect();
        if at.is_empty() {
            return;
        }
        for (id, (stop, _)) in &self.mirror_wait {
            if at.contains(stop) && !self.claimed.contains_key(id) {
                self.claimed.insert(*id, now);
                self.claims_out.push(*id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Off a bus 3 m from the pavement's path (along y), walking onto it at 1.1 m/s: hardly
    /// faster over any quarter of a second (pulled by the corridor all the way, over 1.5
    /// m/s), and on the path in the end.
    #[test]
    fn off_a_bus_people_walk_onto_the_pavement_at_their_pace() {
        let walk = |held: bool| {
            let pace = 1.1;
            let mut w = Walker::new(DVec2::new(3.0, 0.0), 0.25, 0);
            let params = CrowdParams::default();
            let dt = 1.0 / 60.0;
            let mut track = vec![w.pos];
            for _ in 0..360 {
                let s = w.pos.y.max(0.0);
                let (a, b) = (DVec2::new(0.0, s - 2.0), DVec2::new(0.0, s + 2.5));
                w.want = (DVec2::new(0.0, s + 1.3) - w.pos).normalize_or_zero() * pace;
                let c = Some((a, b, 0.5));
                w.corridor = if held { c } else { path_corridor(w.pos, c) };
                crowd::step(std::slice::from_mut(&mut w), &[], &params, dt);
                track.push(w.pos);
            }
            let fastest = track.windows(16).map(|k| (k[15] - k[0]).length() / (15.0 * dt)).fold(0.0, f64::max);
            (fastest, w.pos)
        };
        let (fastest, end) = walk(false);
        assert!(fastest < 1.3, "{fastest:.2} m/s");
        assert!(end.x.abs() < 0.55, "{end:?}");
        let held = walk(true).0;
        assert!(held > 1.5, "{held:.2} m/s");
        // on the path the corridor holds again
        assert!(path_corridor(DVec2::new(0.55, 0.0), Some((DVec2::ZERO, DVec2::Y, 0.5))).is_some());
        assert!(path_corridor(DVec2::new(3.0, 0.0), Some((DVec2::ZERO, DVec2::Y, 0.5))).is_none());
    }

    #[test]
    fn map_humans_load_nested_paths_and_preserve_weights() {
        let root = std::env::temp_dir().join(format!(
            "omsi-map-human-paths-{}", std::process::id()
        ));
        let nested = root.join("Humans/JP_Test/Child_1");
        std::fs::create_dir_all(&nested).unwrap();
        // Synthetic definitions: no original passenger assets are required.
        std::fs::write(nested.join("Child_1.hum"), "[model]\nmodel.cfg\n").unwrap();
        std::fs::write(nested.join("model.cfg"), "").unwrap();
        let other = root.join("Humans/Other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("Man.hum"), "[model]\nmodel.cfg\n").unwrap();
        std::fs::write(other.join("model.cfg"), "").unwrap();
        let list = vec![
            "humans\\jp_test\\child_1\\child_1.hum".into(),
            "Humans/JP_Test/Child_1/Child_1.hum".into(),
            "JP_Test/Child_1/Child_1.hum".into(),
            "Humans/JP_Test/Missing.hum".into(),
        ];
        let picked = map_human_types(&root, &list);
        assert_eq!(picked.len(), 3);
        assert!(Arc::ptr_eq(&picked[0], &picked[1]));
        assert!(Arc::ptr_eq(&picked[1], &picked[2]));
        // (case-blind: a case-insensitive disk keeps the list's own spelling; and
        // separator-blind: on Windows `nested` keeps the slashes it was joined with, while
        // the resolved path is built with backslashes)
        let lower = |p: &Path| p.to_string_lossy().to_lowercase().replace('\\', "/");
        assert!(picked.iter().all(|t| lower(&t.def.path).starts_with(&lower(&nested))));
        assert!(map_human_types(&root, &["Humans/Missing/None.hum".into()]).is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Berlin 1991's pack: full fare, short haul, day ticket (adults), and two reduced
    /// fares for 6..13.
    fn berlin_91() -> omsi_content::tickets::TicketPack {
        let t = |name: &str, age: (i32, i32), day: bool, p: f32| omsi_content::tickets::Ticket {
            name: name.into(),
            age_min: age.0,
            age_max: age.1,
            day_ticket: day,
            probability: p,
            ..Default::default()
        };
        omsi_content::tickets::TicketPack {
            stamper_prop: 0.3,
            ticketbuy_prop: 0.2,
            tickets: vec![
                t("Fahrschein", (14, 200), false, 1.0),
                t("Kurzstrecke", (14, 200), false, 0.4),
                t("Tageskarte", (14, 200), true, 0.2),
                t("Ermaessigt", (6, 13), false, 1.0),
                t("Kurzstrecke Erm", (6, 13), false, 0.4),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn seats_counted_by_the_scripts_numbers() {
        let seat = |omsi_seat: usize| Seat { pos: Vec3::ZERO, floor: Vec3::ZERO, rot: 0.0, seated: true, height: 0.45, omsi_seat };
        // the driver's place is seat 0, a second section's numbers follow the first's
        let seats = [seat(1), seat(2), seat(4), seat(6)];
        assert_eq!(seat_numbers(&seats, [0, 2, 2, 3].into_iter()), [0, 1, 0, 0, 2, 0, 1]);
        assert!(seat_numbers(&[], [0].into_iter()).is_empty());
    }

    #[test]
    fn tickets_by_age_and_time() {
        let mut h = Humans::new(Path::new("/nonexistent"));
        h.tickets = Some(Arc::new(berlin_91()));
        let count = |h: &mut Humans, age: f32| {
            let mut n = [0usize; 5];
            for _ in 0..4000 {
                n[h.pick_ticket(age).unwrap()] += 1;
            }
            n
        };
        // an adult (OMSI's default age of 40) never gets a reduced fare, a child only those
        h.time_of_day = 9.0 * 3600.0;
        let adult = count(&mut h, 40.0);
        assert_eq!(adult[3] + adult[4], 0);
        assert!(adult[2] > 300, "{adult:?}");
        let child = count(&mut h, 10.0);
        assert_eq!(child[0] + child[1] + child[2], 0);
        // day tickets sell best at 9:00, little early in the morning and late at night
        h.time_of_day = 1.0 * 3600.0;
        let early = count(&mut h, 40.0);
        assert!(early[2] * 4 < adult[2], "{early:?} vs {adult:?}");
        assert!(day_ticket_factor(9.0 * 3600.0) > 0.99);
        assert!(day_ticket_factor(0.0) < 0.01);
        assert!((day_ticket_factor(20.0 * 3600.0) - (1.0 - 39_600.0 / 56_376.0) as f32).abs() < 1e-3);
        // nobody in the age range: no ticket
        assert_eq!(h.pick_ticket(3.0), None);
    }

    fn lane(points: Vec<DVec3>, kind: LaneKind) -> omsi_sim::traffic::Lane {
        omsi_sim::traffic::LaneBuilder::polyline(points, kind, 2.5)
    }

    #[test]
    fn pavement_corners_are_joined_and_routed() {
        // an L of pavement: the two paths meet at a right angle, which the road network's
        // heading rule leaves unlinked
        let mut net = Network::default();
        net.lanes.push(lane(
            vec![DVec3::new(0.0, 0.0, 0.0), DVec3::new(0.0, 20.0, 0.0)],
            LaneKind::Sidewalk,
        ));
        net.lanes.push(lane(
            vec![DVec3::new(0.5, 20.3, 0.0), DVec3::new(30.0, 20.3, 0.0)],
            LaneKind::Sidewalk,
        ));
        net.lanes.push(lane(
            vec![DVec3::new(-5.0, 10.0, 0.0), DVec3::new(5.0, 10.0, 0.0)],
            LaneKind::Street,
        ));
        net.link(1.5);
        assert!(
            net.lanes[0].next.is_empty(),
            "the road rule does not join the corner"
        );
        let ped = PedNet::build(&net);
        // the corner is one junction: walking up the first path goes on round it
        let up = Leg { lane: 0, a: 5.0, b: 20.0 };
        let corner = ped.end_node(&net, &up).unwrap();
        assert!(ped.out[corner].iter().any(|&(l, fwd)| l == 1 && fwd), "{:?}", ped.out[corner]);
        // a dead end turns round
        let n = ped
            .end_node(
                &net,
                &Leg {
                    lane: 1,
                    a: 0.0,
                    b: net.lanes[1].length(),
                },
            )
            .unwrap();
        let turn = ped.next_leg(&net, n, 1, 7).unwrap();
        assert_eq!(turn.lane, 1);
        assert!(turn.a > turn.b);
    }

    #[test]
    fn crossings_of_a_pavement_path_are_found() {
        let mut net = Network::default();
        net.lanes.push(lane(
            vec![DVec3::new(0.0, 0.0, 0.0), DVec3::new(0.0, 8.0, 0.0)],
            LaneKind::Sidewalk,
        ));
        net.lanes.push(lane(
            vec![DVec3::new(-30.0, 4.0, 0.0), DVec3::new(30.0, 4.0, 0.0)],
            LaneKind::Street,
        ));
        net.link(1.5);
        let mut ped = PedNet::build(&net);
        let x = ped.crossings(&net, 0).to_vec();
        assert_eq!(x.len(), 1);
        assert!((x[0] - DVec2::new(0.0, 4.0)).length() < 1e-6);
    }

    /// An articulated bus: the front section's cabin and the rear section's (which only has
    /// exits and a seat) become one network through the joint, numbered front first, and a
    /// walk through the bent joint moves on without a jump.
    #[test]
    fn articulated_cabins_are_joined_through_the_bellows() {
        let dir = std::env::temp_dir().join(format!("omsi-humans-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, text: &str| std::fs::write(dir.join(name), text).unwrap();
        // front: door 0 at the front right, exit 1 in the middle, link to the rear at 3
        write("paths_a.cfg", "[pathpnt]\n1.2\n4\n0.4\n[pathpnt]\n0\n4\n0.5\n[pathpnt]\n0\n0\n0.5\n[pathpnt]\n0\n-4.2\n0.6\n[pathpnt]\n1.2\n0\n0.4\n[pathlink]\n0\n1\n[pathlink]\n1\n2\n[pathlink]\n2\n3\n[pathlink]\n2\n4\n");
        write(
            "cabin_a.cfg",
            "[entry]\n0\n[exit]\n4\n[linkToPrevVeh]\n3\n[passpos]\n-0.5\n2\n1.0\n0.45\n0\n",
        );
        // rear: only exits, a seat and the link to the front at point 0
        write("paths_b.cfg", "[pathpnt]\n0\n3.6\n0.6\n[pathpnt]\n0\n0\n0.6\n[pathpnt]\n1.2\n0\n0.4\n[pathpnt]\n0\n-2\n0.6\n[pathlink]\n0\n1\n[pathlink]\n1\n2\n[pathlink]\n1\n3\n");
        write(
            "cabin_b.cfg",
            "[exit]\n2\n[linkToNextVeh]\n0\n[passpos]\n-0.5\n-2\n1.1\n0.45\n0\n",
        );
        let def = |cabin: &str, paths: &str| omsi_vehicle::Vehicle {
            path: dir.join("bus.bus"),
            passenger_cabin: Some(cabin.into()),
            paths: Some(paths.into()),
            bounding_box: Some([2.5, 9.0, 3.0, 0.0, 0.0, 1.5]),
            ..Default::default()
        };
        let (front, rear) = (
            def("cabin_a.cfg", "paths_a.cfg"),
            def("cabin_b.cfg", "paths_b.cfg"),
        );
        // couplings: the front's at y -4.3, the rear's own at y 4.0
        let (back, own) = (Vec3::new(0.0, -4.3, 0.3), Vec3::new(0.0, 4.0, 0.3));
        let offset = back - own;
        let cabin =
            Cabin::load_train(&[(&front, Vec3::ZERO, f32::INFINITY), (&rear, offset, back.y)])
                .expect("cabin");
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(cabin.parts.len(), 2);
        assert_eq!(cabin.graph.points.len(), 9);
        assert_eq!(
            (cabin.entries.len(), cabin.exits.len(), cabin.seats.len()),
            (1, 2, 2)
        );
        // exit 1 is the rear section's door, where the rear file puts it
        assert!(
            (cabin.exits[1].inside - Vec3::new(1.2, -8.3, 0.4)).length() < 1e-4,
            "{:?}",
            cabin.exits[1].inside
        );
        // the seat in the rear is reached from the front door through the joint, along the
        // routing tables Omsi.exe builds over the joined network
        let seat = &cabin.seats[1];
        assert!((seat.pos.y + 10.3).abs() < 1e-4);
        let all: Vec<Option<usize>> = (0..cabin.graph.points.len()).map(Some).collect();
        let to = cabin.omsi_nearest(seat.floor, &all, false, false, None, None).unwrap();
        let mut at = cabin.entries[0].point.unwrap();
        let mut route = vec![cabin.graph.points[at]];
        while at != to {
            at = cabin.route_next(at, to).expect("a way on").0;
            route.push(cabin.graph.points[at]);
            assert!(route.len() < 20, "{route:?}");
        }
        assert!(
            route.iter().any(|p| (p.y + 4.2).abs() < 1e-4)
                && route.iter().any(|p| (p.y + 4.7).abs() < 1e-4),
            "{route:?}"
        );
        // and the nearest exit from there is the rear one
        let exit = cabin.omsi_nearest(seat.floor, &cabin.exit_points(), false, false, None, None);
        assert_eq!(exit, cabin.exits[1].point);
        // the rear section bent 30 degrees about the coupling: walking down the aisle moves on
        // smoothly, and the frames agree with the sections away from the joint
        let lead_rot = Mat4::IDENTITY;
        let bent = 30.0f64;
        let rot = Mat4::from_rotation_z((-bent).to_radians() as f32);
        let pos = back.as_dvec3() - rot.transform_point3(own).as_dvec3();
        let frames = [PartFrame {
            pos,
            rot,
            heading: bent,
            offset,
            joint_y: back.y,
            half: DVec2::new(1.25, 4.5),
            centre: DVec2::ZERO,
        }];
        // beside the aisle the two frames disagree by 0.31 m at the joint itself
        let at_joint = Vec3::new(0.6, back.y, 0.5);
        let rear_frame = pos + rot.transform_point3(at_joint - offset).as_dvec3();
        assert!((rear_frame - at_joint.as_dvec3()).length() > 0.3);
        let mut last = train_point(DVec3::ZERO, &lead_rot, &frames, Vec3::new(0.6, 0.0, 0.5));
        for k in 1..=100 {
            let y = -(k as f32) * 0.1;
            let p = train_point(DVec3::ZERO, &lead_rot, &frames, Vec3::new(0.6, y, 0.5));
            assert!(
                (p - last).length() < 0.14,
                "a jump of {:.3} m at y {y}",
                (p - last).length()
            );
            last = p;
        }
        let ahead = train_point(DVec3::ZERO, &lead_rot, &frames, Vec3::new(1.0, -1.0, 0.5));
        assert!((ahead - DVec3::new(1.0, -1.0, 0.5)).length() < 1e-4);
        let behind_joint = Vec3::new(1.0, -9.0, 0.5);
        let p = train_point(DVec3::ZERO, &lead_rot, &frames, behind_joint);
        assert!(
            (p - (pos + rot.transform_point3(behind_joint - offset).as_dvec3())).length() < 1e-4
        );
        assert!((train_heading(0.0, &frames, behind_joint) - bent).abs() < 1e-9);
        assert!(
            (train_heading(0.0, &frames, Vec3::new(0.0, back.y, 0.5)) - bent * 0.5).abs() < 1e-9
        );
    }

    /// The SD200's footsteps as its paths.cfg gives them to the links (#311): the stairs
    /// sound as stairs, the front of the upper deck as its own floor, the aisle below as
    /// the plain floor.
    #[test]
    fn footsteps_come_from_the_links_step_sound_pack() {
        let root = omsi_cfg::env::var_os("OMSI_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("../../../OMSI 2 Original"));
        let bus = root.join("Vehicles/MAN_SD200/MAN_SD80.bus");
        if !bus.exists() {
            eprintln!("skipped: no {}", bus.display());
            return;
        }
        let def = omsi_vehicle::Vehicle::load(&bus).expect("SD200");
        let cabin = Cabin::load_train(&[(&def, Vec3::ZERO, f32::INFINITY)]).expect("cabin");
        // the link nearest the point, and its pack
        let first = |p: Vec3| {
            let pts = &cabin.graph.points;
            let d = |l: &(i32, i32, bool)| {
                let (a, b) = (pts[l.0 as usize], pts[l.1 as usize]);
                let ab = b - a;
                let t = ((p - a).dot(ab) / ab.length_squared().max(1e-6)).clamp(0.0, 1.0);
                (a + ab * t - p).length()
            };
            let l = (0..cabin.links.len()).min_by(|&x, &y| d(&cabin.links[x]).total_cmp(&d(&cabin.links[y])))?;
            cabin.link_pack[l].map(|k| cabin.step_packs[k][0].to_ascii_lowercase())
        };
        assert_eq!(first(Vec3::new(-0.89, -1.61, 1.63)).as_deref(), Some("step_st_01.wav"), "the rear stairs");
        assert_eq!(first(Vec3::new(0.0, 4.35, 2.5)).as_deref(), Some("step_ov_01.wav"), "the upper deck's front");
        assert_eq!(first(Vec3::new(0.0, 0.84, 0.57)).as_deref(), Some("step_01.wav"), "the aisle below");
    }

    /// The SD202's cabin: the stairs down from the upper deck end beside the rear exits, and
    /// the walk from up there to an exit goes down the stairs.
    #[test]
    fn double_decker_exits_are_reached_down_the_stairs() {
        let root = omsi_cfg::env::var_os("OMSI_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("../../../OMSI 2 Original"));
        let bus = root.join("Vehicles/MAN_SD202/MAN_D92.bus");
        if !bus.exists() {
            eprintln!("skipped: no {}", bus.display());
            return;
        }
        let def = omsi_vehicle::Vehicle::load(&bus).expect("SD202");
        let cabin = Cabin::load_train(&[(&def, Vec3::ZERO, f32::INFINITY)]).expect("cabin");
        assert_eq!(cabin.exits.len(), 2);
        let exit = &cabin.exits[1];
        assert!(
            (exit.wait - Vec3::new(0.806, -1.26, 0.505)).length() < 1e-3,
            "{:?}",
            exit.wait
        );
        // from the upper deck the routing tables lead down both flights to the exit
        let all: Vec<Option<usize>> = (0..cabin.graph.points.len()).map(Some).collect();
        let upstairs = cabin.omsi_nearest(Vec3::new(0.0, -1.8, 2.46), &all, false, false, None, None).unwrap();
        assert!((cabin.graph.points[upstairs].z - 2.46).abs() < 0.1);
        let to = exit.point.unwrap();
        let mut at = upstairs;
        let mut route = vec![cabin.graph.points[at]];
        while at != to {
            at = cabin.route_next(at, to).expect("a way down").0;
            route.push(cabin.graph.points[at]);
            assert!(route.len() < 60, "{route:?}");
        }
        assert!(
            route.iter().any(|p| (p.z - 1.82).abs() < 0.01)
                && route.iter().any(|p| (p.z - 1.205).abs() < 0.01),
            "{route:?}"
        );
    }

    #[test]
    fn legs_run_both_ways() {
        let mut net = Network::default();
        net.lanes.push(lane(
            vec![DVec3::new(0.0, 0.0, 0.0), DVec3::new(0.0, 10.0, 0.0)],
            LaneKind::Sidewalk,
        ));
        let back = Leg {
            lane: 0,
            a: 8.0,
            b: 2.0,
        };
        let (p, h) = back.at(&net, 1.0);
        assert!((p.y - 7.0).abs() < 1e-6);
        assert!((h - 180.0).abs() < 1e-6);
        assert!((back.project(&net, DVec3::new(0.3, 5.0, 0.0), 2.5) - 3.0).abs() < 0.11);
    }

    #[test]
    fn doors_open_falls_back_when_exit_vars_are_undeclared() {
        let dir = std::env::temp_dir().join(format!("omsi-doors-open-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("test.bus"),
            "[model]\nmodel.cfg\n[varnamelist]\n1\nvars.txt\n[script]\n1\nmain.osc\n",
        )
        .unwrap();
        std::fs::write(dir.join("model.cfg"), "").unwrap();
        // Front door leaf 0 uses PAX_Entry0_Open. Rear door (door_2) has no PAX_Exit0_Open in varlist.
        std::fs::write(dir.join("vars.txt"), "door_0\ndoor_1\ndoor_2\nPAX_Entry0_Open\n").unwrap();
        std::fs::write(dir.join("main.osc"), "{init}\n{end}\n").unwrap();

        let ty = std::sync::Arc::new(omsi_sim::VehicleType::load(&dir, &dir.join("test.bus")).unwrap());
        let mut v = VehicleInstance::new(ty, omsi_sim::VehicleHost::new(Default::default()));

        // Initially both entries and exit closed
        let (e, x) = Humans::doors_open(&v, 2, 1);
        assert_eq!(e, vec![false, false]);
        assert_eq!(x, vec![false]);

        // Front door leaf 0 opens via PAX_Entry0_Open
        v.set_var("PAX_Entry0_Open", 1.0);
        let (e, x) = Humans::doors_open(&v, 2, 1);
        assert_eq!(e, vec![true, false]);
        assert_eq!(x, vec![false]);

        // Rear door leaf 2 opens (falls back to door_2 since PAX_Exit0_Open is not in varlist)
        v.set_var("door_2", 1.0);
        let (e, x) = Humans::doors_open(&v, 2, 1);
        assert_eq!(e, vec![true, false]);
        assert_eq!(x, vec![true]);

        // Front door leaf 1 opens via door_1 fallback
        v.set_var("door_1", 1.0);
        let (e, x) = Humans::doors_open(&v, 2, 1);
        assert_eq!(e, vec![true, true]);
        assert_eq!(x, vec![true]);

        std::fs::remove_dir_all(&dir).ok();
    }


    #[test]
    fn doors_open_reads_pax_vars_the_script_writes_without_declaring() {
        let dir = std::env::temp_dir().join(format!("omsi-doors-open-undeclared-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("test.bus"),
            "[model]\nmodel.cfg\n[varnamelist]\n1\nvars.txt\n[script]\n1\nmain.osc\n",
        )
        .unwrap();
        std::fs::write(dir.join("model.cfg"), "").unwrap();
        std::fs::write(dir.join("vars.txt"), "door_0\n").unwrap();
        std::fs::write(dir.join("main.osc"), "{frame}\n1 (S.L.PAX_Entry0_Open)\n{end}\n").unwrap();

        let ty = std::sync::Arc::new(omsi_sim::VehicleType::load(&dir, &dir.join("test.bus")).unwrap());
        let mut v = VehicleInstance::new(ty, omsi_sim::VehicleHost::new(Default::default()));
        v.set_var("door_0", 0.0);
        v.set_var("PAX_Entry0_Open", 1.0);
        let (e, _) = Humans::doors_open(&v, 1, 0);
        assert_eq!(e, vec![true]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn doors_open_3door_bus_handles_middle_and_rear_exits() {
        let dir = std::env::temp_dir().join(format!("omsi-doors-3door-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("test.bus"),
            "[model]\nmodel.cfg\n[varnamelist]\n1\nvars.txt\n[script]\n1\nmain.osc\n",
        )
        .unwrap();
        std::fs::write(dir.join("model.cfg"), "").unwrap();
        std::fs::write(dir.join("vars.txt"), "door_0\ndoor_1\ndoor_2\ndoor_3\ndoor_4\ndoor_5\n").unwrap();
        std::fs::write(dir.join("main.osc"), "{init}\n{end}\n").unwrap();

        let ty = std::sync::Arc::new(omsi_sim::VehicleType::load(&dir, &dir.join("test.bus")).unwrap());
        let mut v = VehicleInstance::new(ty, omsi_sim::VehicleHost::new(Default::default()));

        // 3-door bus: 6 entries (all 3 doors), 4 exits (middle door leaves 2,3; rear door leaves 4,5)
        let (e, x) = Humans::doors_open(&v, 6, 4);
        assert_eq!(e, vec![false; 6]);
        assert_eq!(x, vec![false; 4]);

        // Middle doors (door_2 and door_3) open
        v.set_var("door_2", 1.0);
        v.set_var("door_3", 1.0);
        let (e, x) = Humans::doors_open(&v, 6, 4);
        assert_eq!(e, vec![false, false, true, true, false, false]);
        assert_eq!(x, vec![true, true, false, false]);

        // Rear doors (door_4 and door_5) open
        v.set_var("door_4", 1.0);
        v.set_var("door_5", 1.0);
        let (e, x) = Humans::doors_open(&v, 6, 4);
        assert_eq!(e, vec![false, false, true, true, true, true]);
        assert_eq!(x, vec![true, true, true, true]);

        std::fs::remove_dir_all(&dir).ok();
    }
}


/// A heading in 0..360 degrees.
/// A line a passenger says: the sample and where they stand.
pub struct VoiceLine {
    pub position: DVec3,
    pub path: std::path::PathBuf,
}

/// How much of its probability a day ticket keeps at a time of day (seconds): rising from
/// nothing at midnight to all of it at 9:00, as the ticket packs describe it, then falling
/// on OMSI's line.
fn day_ticket_factor(t: f64) -> f32 {
    let t = t.rem_euclid(86_400.0);
    let rise = t / 32_400.0;
    let fall = 1.0 - (t - 32_400.0) / (88_776.0 - 32_400.0);
    rise.min(fall).clamp(0.0, 1.0) as f32
}

fn wrap_heading(h: f64) -> f64 {
    h.rem_euclid(360.0)
}

/// The angle between two headings (degrees, 0..180).
fn angle_between(a: f64, b: f64) -> f64 {
    ((b - a + 540.0).rem_euclid(360.0) - 180.0).abs()
}
