//! Placement of the map's attached objects, the map-wide index, and tile streaming.
//!
//! * `[attachObj]` hangs an object on one of the `[new_attachment]` points of its parent
//!   (traffic lights on a whip beam, line plates on a bus stop sign, the next house of a
//!   terrace) - none of them was placed before, which is most of what "objects missing" and
//!   "a sign floating in the air" were on the stock maps.
//! * `[splineAttachement]` puts a row of objects along a spline (street lamps, parking
//!   bays, catenary masts) and the splines after it in its tile;
//!   `[splineAttachement_repeater]` continues the row where the chain enters another tile,
//!   and the position there depends on the length of the chain from its start - which may
//!   lie in other tiles. [`MapIndex`] knows every spline of the map for that walk, and every
//!   object for entry points and bus stops of tiles that are not loaded.
//! * [`Streamer`] loads the tiles around the camera and the player's bus on a worker
//!   thread, uploads what they produced a little each frame and unloads the far ones.

use glam::{DVec2, DVec3, Mat4, Vec3};
use hashbrown::{HashMap, HashSet};
use omsi_geometry::SplineCurve;
use omsi_map::{tile_size, MapSpline, SplineAttachment, Tile};
use rayon::prelude::*;
use std::path::{Path, PathBuf};

/// A spline of the map as far as a repeater's walk along its chain needs it.
#[derive(Debug, Clone, Copy)]
pub struct IndexedSpline {
    pub length: f64,
    /// The chain distance stored in the last `[spline]` field (tile version 11+).
    /// `None` for older tiles, where that field is not present.
    pub map_chain_offset: Option<f64>,
    pub prev: i64,
    pub next: i64,
}

impl IndexedSpline {
    fn from_map(spline: &MapSpline) -> Self {
        Self {
            length: spline.length,
            map_chain_offset: spline.map_chain_offset,
            prev: spline.prev_id,
            next: spline.next_id,
        }
    }
}

/// What the map holds outside the loaded tiles: its splines (lengths, chain offsets and links),
/// the spline every `[splineAttachement]` row starts on, and where every object stands.
#[derive(Default)]
pub struct MapIndex {
    pub splines: HashMap<i64, IndexedSpline>,
    /// (tile index in global.cfg, attachment id) → (spline id, distance of the row's first
    /// object from the start of that spline - negative when it lies before it).
    pub masters: HashMap<(usize, i64), (i64, f64)>,
    /// Object id → (tile, world position with the ground under it, rotation).
    pub objects: HashMap<i64, ((i32, i32), DVec3, [f64; 3])>,
    /// The objects whose id another tile uses as well, by (tile, id): a map joined from two
    /// maps (two towns you cannot drive between) repeats ids, and an entry point named by
    /// its id was looked for in the other town.
    pub duplicates: HashMap<((i32, i32), i64), (DVec3, [f64; 3])>,
    /// Every object and spline file the map names (as written, lower case), with the number
    /// of records naming it and one tile that does.
    pub files: HashMap<String, (usize, (i32, i32))>,
    /// Crossing ids whose light program runs: placed `[trafficlight]` objects name them, or
    /// a child of any kind names one of their lights (`names_traffic_light`), including
    /// children on other tiles.
    pub traffic_light_parents: HashSet<i64>,
    /// Tile → the world rectangle (x0, y0, x1, y1) its tile square and its splines (with
    /// room for their width) cover.
    pub covers: HashMap<(i32, i32), [f64; 4]>,
    pub tiles_read: usize,
    pub tiles_failed: usize,
    /// Object id → how many passengers get off at it, as Omsi.exe weighs a `[busstop]`'s
    /// strings (see [`stop_exit_weight`]); only objects that carry strings.
    pub stop_weights: HashMap<i64, f32>,
    /// Object id → its `[busstop]`'s (pass_enter_max, pass_enter_min) (see
    /// [`stop_enter`]); only objects that carry strings.
    pub stop_enter: HashMap<i64, (f32, f32)>,
    /// Object id → the side the stop's platform lies on (see [`stop_side`]): 0 = right of
    /// the way the map lays the road (the side a bus stopping in its own lane keeps its
    /// doors on), 1 = the other. Only objects that carry strings.
    pub stop_side: HashMap<i64, f32>,
    /// Object id → the stop's length (string 4, 30 m when not given; Omsi.exe's +0x7c,
    /// sub_620058): how far along it the waiting places and a standing bus may be.
    pub stop_length: HashMap<i64, f32>,
}

/// How many passengers get off at a bus stop, as Omsi.exe reads the stop object's strings
/// when it sets the station up (0x620058): `pass_enter_max` (string 1, else 1) and
/// `pass_enter_min` (string 2, else 0) rounded, and `pass_exit` (string 3) rounded - or,
/// without it, the mean of the two - never below 0. A boarding passenger draws where to get
/// off among the stops ahead by these numbers (0x61baa8), so a stop with twice the number
/// takes twice the riders; the stock maps put 10 on every stop.
pub fn stop_exit_weight(strings: &[String]) -> f32 {
    let (max, min) = stop_enter(strings);
    stop_num(strings, 3).unwrap_or((min + max) / 2.0).max(0.0)
}

fn stop_num(strings: &[String], i: usize) -> Option<f32> {
    strings.get(i).map(|s| s.trim()).filter(|s| !s.is_empty()).and_then(|s| s.replace(',', ".").parse::<f64>().ok()).filter(|v| v.is_finite()).map(|v| v.round() as f32)
}

/// A bus stop's `pass_enter_max` (string 1, else 1) and `pass_enter_min` (string 2, else
/// 0), rounded, as Omsi.exe sets the station up (0x620058): how many people wait there.
pub fn stop_enter(strings: &[String]) -> (f32, f32) {
    (stop_num(strings, 1).unwrap_or(1.0), stop_num(strings, 2).unwrap_or(0.0))
}

/// The side a bus stop's platform lies on, as Omsi.exe reads it off the stop object's
/// *timetable data* strings: string 5 (0-based, after name and the entering/exiting
/// numbers), 0 = the right of the way the map lays the road down, 1 = the other side.
///
/// AiList vehicles that carry doors on both sides (Urumqi61's `[AI]YoungMan*`) read it as
/// `AI_Scheduled_AtStation_Side` and open only the platform's doors; without it a left-hand
/// platform is served through the traffic, and the door lamps on that side stay dark.
///
/// Anything else (an empty string, rubbish) means the map says nothing, which OMSI takes
/// as the right-hand side: 0. Stops whose strings are not timetable data at all (Spandau's
/// `bss1\*.jpg` entry-point signs carry flag 7 too) land on the same default. A value past
/// 1 (OMSI's door scripts test `= 1`, so their other branch covers everything else) is kept
/// as it stands, up to the two a script that knows the sides can tell apart.
pub fn stop_length(strings: &[String]) -> f32 {
    strings.get(4).map(|s| s.trim()).filter(|s| !s.is_empty()).and_then(|s| s.replace(',', ".").parse::<f64>().ok()).filter(|v| v.is_finite()).map(|v| v as f32).unwrap_or(30.0)
}

pub fn stop_side(strings: &[String]) -> f32 {
    strings.get(5).map(|s| s.trim()).and_then(|s| s.parse::<f64>().ok()).filter(|v| v.is_finite()).map(|v| v.clamp(0.0, 2.0) as f32).unwrap_or(0.0)
}

/// Does a child of a crossing name one of its lights? OMSI's `RefreshAmpelParenting`
/// (0x77d460) switches a crossing's light program on for every object whose `[varparent]`
/// is that crossing and whose first string is a light index (`StrToInt` >= 0) - whatever
/// kind of object it is: it never asks for `[trafficlight]`. A mod's lamp without that
/// keyword (a copy of a lamp with its own script) still runs its junction, and the lamp
/// shows that light.
pub fn names_traffic_light(strings: &[String]) -> bool {
    strings.first().and_then(|s| s.trim().parse::<i64>().ok()).is_some_and(|i| i >= 0)
}

fn traffic_light_parents(tile: &Tile, mut is_signal: impl FnMut(&str) -> bool) -> HashSet<i64> {
    light_children(tile)
        .filter_map(|(file, _, parent)| is_signal(file).then_some(parent))
        .collect()
}

/// Every child of another object in a tile: (file, strings, parent id).
fn light_children(tile: &Tile) -> impl Iterator<Item = (&str, &[String], i64)> {
    tile.objects.iter().chain(&tile.attach_objects)
        .filter_map(|o| o.var_parent.or(o.parent_id).map(|parent| (o.file.as_str(), o.extra.as_slice(), parent)))
        .chain(tile.spline_attachments.iter().filter_map(|a| a.var_parent.map(|parent| (a.file.as_str(), a.strings.as_slice(), parent))))
}

/// The parents a tile's children name a light of (`names_traffic_light`), whatever the
/// children are: whether the parent is a crossing with a light program is settled once
/// the whole map is read (it may stand on another tile).
fn light_naming_parents(tile: &Tile) -> HashSet<i64> {
    light_children(tile)
        .filter_map(|(_, strings, parent)| names_traffic_light(strings).then_some(parent))
        .collect()
}

impl MapIndex {
    /// Read every tile file of the map (with the active chrono patches), without meshes.
    /// Signal types are read only to identify installed traffic lights. The tiles are read
    /// on the worker pool; a tile that cannot be read is logged and left out. `tiles` are
    /// (index in global.cfg's `[map]` list, x, y, file): repeaters and timetable tracks name
    /// a tile by that index, which a missing tile file must not shift.
    pub fn build(tiles: &[(usize, i32, i32, PathBuf)], chrono_dirs: &[PathBuf], root: &Path) -> MapIndex {
        /// What one tile adds besides its own index part: its rows (key, spline, start
        /// distance, interval), its repeaters (master key, spline, first object index),
        /// and parent attachments to place after the spline rows.
        type RowParts = (
            Vec<((usize, i64), i64, f64, f64, IndexedSpline)>,
            Vec<((usize, i64), i64, usize)>,
            Vec<((i32, i32), MapSpline, SplineAttachment)>,
            Vec<omsi_map::MapObject>,
        );
        let t0 = std::time::Instant::now();
        let signal_types = parking_lot::Mutex::new(HashMap::new());
        // (per tile: the parents its children name a light of, and its objects' files - the
        // objects by id, the files once each - to tell afterwards which of those parents
        // are crossings with a light program)
        type LightParts = (HashSet<i64>, Vec<(i64, u32)>, Vec<String>);
        let parts: Vec<Option<(MapIndex, RowParts, LightParts)>> = tiles
            .par_iter()
            .map(|(gi, tx, ty, path)| {
                let tile = read_tile(path, chrono_dirs)?;
                let mut part = MapIndex::default();
                part.traffic_light_parents = traffic_light_parents(&tile, |file| {
                    let key = file.replace('/', "\\").to_ascii_lowercase();
                    *signal_types.lock().entry(key).or_insert_with(|| {
                        let path = omsi_cfg::resolve_path(root, file);
                        match omsi_scenery::SceneryObject::load(&path) {
                            Ok(sco) => sco.is_traffic_light,
                            Err(e) => {
                                log::warn!("reading traffic light type: {e}");
                                false
                            }
                        }
                    })
                });
                let lights: LightParts = {
                    let named = light_naming_parents(&tile);
                    let mut names: Vec<String> = Vec::new();
                    let mut by_name: HashMap<&str, u32> = HashMap::new();
                    let mut ids = Vec::with_capacity(tile.objects.len());
                    for o in &tile.objects {
                        let k = *by_name.entry(o.file.as_str()).or_insert_with(|| {
                            names.push(o.file.clone());
                            names.len() as u32 - 1
                        });
                        ids.push((o.id, k));
                    }
                    (named, ids, names)
                };
                let mut rows: RowParts = Default::default();
                for s in tile.splines.iter().filter(|s| !s.deleted) {
                    part.splines.insert(s.id, IndexedSpline::from_map(s));
                }
                for a in &tile.spline_attachments {
                    let Some(s) = tile.splines.get(a.spline_index.max(0) as usize) else { continue };
                    match a.repeater {
                        None => {
                            rows.0.push(((*gi, a.id), s.id, a.offset[2], a.interval, IndexedSpline::from_map(s)));
                            if !s.deleted {
                                rows.2.push(((*tx, *ty), s.clone(), SplineAttachment {
                                    file: a.file.clone(),
                                    id: a.id,
                                    offset: a.offset,
                                    rot: a.rot,
                                    interval: a.interval,
                                    range: a.range,
                                    tilt: a.tilt,
                                    ..Default::default()
                                }));
                            }
                        }
                        Some((master_tile, first)) => rows.1.push(((master_tile, a.id), s.id, first)),
                    }
                }
                let terrain = omsi_map::Terrain::load(&terrain_file(&tile, path)).ok();
                let origin = DVec2::new(*tx as f64 * tile_size(), *ty as f64 * tile_size());
                part.covers.insert((*tx, *ty), spline_cover(&tile, origin));
                let mut name = |f: &str| {
                    if f.trim().is_empty() {
                        return;
                    }
                    let e = part.files.entry(f.trim().replace('/', "\\").to_ascii_lowercase()).or_insert((0, (*tx, *ty)));
                    e.0 += 1;
                };
                for f in tile.objects.iter().map(|o| &o.file).chain(tile.attach_objects.iter().map(|o| &o.file)).chain(tile.spline_attachments.iter().map(|a| &a.file)).chain(tile.splines.iter().filter(|s| !s.deleted).map(|s| &s.file)) {
                    name(f);
                }
                for o in &tile.objects {
                    let ground = terrain.as_ref().map(|t| t.sample(o.pos[0].clamp(0.0, tile_size()) as f32, o.pos[1].clamp(0.0, tile_size()) as f32) as f64).unwrap_or(0.0);
                    part.objects.insert(o.id, ((*tx, *ty), DVec3::new(origin.x + o.pos[0], origin.y + o.pos[1], o.pos[2] + ground), o.rot));
                    if o.extra.len() >= 2 {
                        part.stop_weights.insert(o.id, stop_exit_weight(&o.extra));
                        part.stop_enter.insert(o.id, stop_enter(&o.extra));
                        part.stop_side.insert(o.id, stop_side(&o.extra));
                        part.stop_length.insert(o.id, stop_length(&o.extra));
                    }
                }
                for a in tile.spline_attachments.iter().filter(|a| a.repeater.is_none() && a.strings.len() >= 2) {
                    part.stop_weights.insert(a.id, stop_exit_weight(&a.strings));
                    part.stop_enter.insert(a.id, stop_enter(&a.strings));
                    part.stop_side.insert(a.id, stop_side(&a.strings));
                    part.stop_length.insert(a.id, stop_length(&a.strings));
                }
                rows.3 = tile.attach_objects;
                part.tiles_read = 1;
                Some((part, rows, lights))
            })
            .collect();
        let mut index = MapIndex::default();
        let mut rows: RowParts = Default::default();
        // A stop's distance counts from the chain's start, often beyond the length of
        // its own segment. All predecessors must be known before indexing row poses,
        // including those on other tiles in maps without an authored chain offset.
        let mut parts = parts;
        for (part, _, _) in parts.iter_mut().flatten() {
            index.splines.extend(std::mem::take(&mut part.splines));
        }
        parts.par_iter_mut().flatten().for_each(|(part, rows, _)| {
            for (tile, spline, attachment) in std::mem::take(&mut rows.2) {
                let origin = DVec2::new(tile.0 as f64 * tile_size(), tile.1 as f64 * tile_size());
                let Some(first) = row_start(&attachment, &spline, Some(&index))
                    .and_then(|start| place_on(&attachment, &spline, origin, Some(&index), start).into_iter().next()) else { continue };
                part.objects.entry(attachment.id).or_insert((tile, first.pose.pos, [first.pose.heading(), 0.0, 0.0]));
            }
            // an object hung on another (`[attachObj]`: the stops of Ahlheim and many
            // other maps hang on their shelters): at its parent's place - a few metres
            // off at most, and the placed object gives the exact place once its tile is
            // loaded. They were in no index at all: a duty's stop beyond the loaded tiles
            // had no place, never showed on the map and was never reached (#1014, #975).
            for o in std::mem::take(&mut rows.3) {
                let Some(parent) = o.parent_id.and_then(|id| part.objects.get(&id).copied()) else { continue };
                part.objects.entry(o.id).or_insert((parent.0, parent.1, [parent.2[0] + o.rot[0], 0.0, 0.0]));
                if o.extra.len() >= 2 {
                    part.stop_weights.insert(o.id, stop_exit_weight(&o.extra));
                    part.stop_enter.insert(o.id, stop_enter(&o.extra));
                    part.stop_side.insert(o.id, stop_side(&o.extra));
                    part.stop_length.insert(o.id, stop_length(&o.extra));
                }
            }
        });
        let named: HashSet<i64> = parts.iter().flatten().flat_map(|p| p.2 .0.iter().copied()).collect();
        let mut programs: HashMap<String, bool> = HashMap::new();
        for (_, ids, names) in parts.iter().flatten().map(|p| &p.2) {
            for &(id, k) in ids {
                if !named.contains(&id) || index.traffic_light_parents.contains(&id) {
                    continue;
                }
                let file = &names[k as usize];
                let key = file.replace('/', "\\").to_ascii_lowercase();
                let has = *programs.entry(key).or_insert_with(|| {
                    omsi_scenery::SceneryObject::load(&omsi_cfg::resolve_path(root, file))
                        .map(|sco| !sco.traffic_lights.is_empty())
                        .unwrap_or(false)
                });
                if has {
                    index.traffic_light_parents.insert(id);
                }
            }
        }
        for p in parts {
            match p {
                Some((p, (r, q, _, _), _)) => {
                    for (id, v) in p.objects {
                        if let Some(prev) = index.objects.get(&id).filter(|prev| prev.0 != v.0) {
                            index.duplicates.insert((prev.0, id), (prev.1, prev.2));
                            index.duplicates.insert((v.0, id), (v.1, v.2));
                        }
                        index.objects.insert(id, v);
                    }
                    index.covers.extend(p.covers);
                    index.stop_weights.extend(p.stop_weights);
                    index.stop_enter.extend(p.stop_enter);
                    index.stop_side.extend(p.stop_side);
                    index.stop_length.extend(p.stop_length);
                    index.traffic_light_parents.extend(p.traffic_light_parents);
                    for (f, (n, t)) in p.files {
                        index.files.entry(f).or_insert((0, t)).0 += n;
                    }
                    index.tiles_read += 1;
                    rows.0.extend(r);
                    rows.1.extend(q);
                }
                None => index.tiles_failed += 1,
            }
        }
        let mut intervals: HashMap<(usize, i64), f64> = HashMap::new();
        for (key, spline, d, interval, local) in rows.0 {
            let offset = chain_offset_from(&index, spline, local);
            index.masters.insert(key, (spline, d - offset));
            intervals.insert(key, interval);
        }
        // how well the chain model matches the editor: a repeater names the first object of
        // the row that lies on its spline
        let (mut checked, mut agree) = (0usize, 0usize);
        for (key, spline, first) in &rows.1 {
            let (Some(&(master_spline, d)), Some(&interval)) = (index.masters.get(key), intervals.get(key)) else { continue };
            if interval <= 0.0 {
                continue;
            }
            if let Some((acc, _)) = chain_distance(&index, master_spline, *spline, f64::MAX) {
                checked += 1;
                let ours = ((acc - d) / interval + 1e-6).ceil().max(0.0) as usize;
                agree += (ours == *first) as usize;
                if ours != *first && omsi_cfg::env::var_os("OMSI_DEBUG_REPEATERS").is_some() {
                    log::info!("repeater {:?} on spline {spline}: the map says object {first}, the chain {ours} (chain {acc:.2} m, start {d:.2} m, interval {interval} m: the map's first at {:.2} m, ours at {:.2} m)", key, d + *first as f64 * interval - acc, d + ours as f64 * interval - acc);
                }
            }
        }
        log::info!("map index: {} tiles read ({} unreadable), {} splines, {} objects, {} attachment rows ({} repeaters, {agree} of the {checked} on a known chain start where the map says), {} object and spline files in {:.2} s", index.tiles_read, index.tiles_failed, index.splines.len(), index.objects.len(), index.masters.len(), rows.1.len(), index.files.len(), t0.elapsed().as_secs_f64());
        index
    }

    /// The files the map names that this installation (with its content folder and archives)
    /// does not have, grouped by the add-on folder they belong to (the folder under
    /// `Sceneryobjects` / `Splines`): (folder, [(file, records, a tile)]).
    pub fn missing_files(&self, root: &Path) -> Vec<(String, Vec<(String, usize, (i32, i32))>)> {
        let named: Vec<(&String, &(usize, (i32, i32)))> = self.files.iter().collect();
        let mut missing: Vec<(String, usize, (i32, i32))> = named
            .into_par_iter()
            .filter(|(f, _)| !omsi_cfg::vfs::is_file(&omsi_cfg::resolve_path(root, f)))
            .map(|(f, (n, t))| (f.clone(), *n, *t))
            .collect();
        missing.sort();
        let mut groups: Vec<(String, Vec<(String, usize, (i32, i32))>)> = Vec::new();
        for m in missing {
            let folder = m.0.split('\\').take(2).collect::<Vec<_>>().join("\\");
            match groups.last_mut() {
                Some((g, list)) if *g == folder => list.push(m),
                _ => groups.push((folder, vec![m])),
            }
        }
        groups
    }
}

/// Room for a spline's width on either side of its centre line.
const SPLINE_HALF_WIDTH: f64 = 50.0;

/// The world rectangle a tile's square and its splines cover. A damaged length is bounded,
/// so one bad record cannot make a tile a neighbour of the whole map.
fn spline_cover(tile: &Tile, origin: DVec2) -> [f64; 4] {
    let ts = tile_size();
    let mut c = [origin.x, origin.y, origin.x + ts, origin.y + ts];
    for s in tile.splines.iter().filter(|s| !s.deleted && !s.file.trim().is_empty()) {
        let curve = SplineCurve::from_map(s, origin);
        let len = if s.length.is_finite() { s.length.clamp(0.0, 5000.0) } else { 0.0 };
        let steps = (len / 10.0).ceil().max(1.0) as usize;
        for i in 0..=steps {
            let p = curve.point_at(len * i as f64 / steps as f64);
            if !(p.x.is_finite() && p.y.is_finite()) {
                continue;
            }
            c = [c[0].min(p.x - SPLINE_HALF_WIDTH), c[1].min(p.y - SPLINE_HALF_WIDTH), c[2].max(p.x + SPLINE_HALF_WIDTH), c[3].max(p.y + SPLINE_HALF_WIDTH)];
        }
    }
    c
}

/// A tile file with the chrono folders active on the sim date applied; None (logged) when
/// the file cannot be read.
pub fn read_tile(path: &Path, chrono_dirs: &[PathBuf]) -> Option<Tile> {
    let mut tile = match Tile::load(path) {
        Ok(t) => t,
        Err(e) => {
            log::warn!("{e}");
            return None;
        }
    };
    if let Some(name) = path.file_name() {
        for c in chrono_dirs {
            let p = omsi_cfg::resolve_path(c, &name.to_string_lossy());
            if !omsi_cfg::vfs::is_file(&p) {
                continue;
            }
            match Tile::load(&p) {
                Ok(patch) => {
                    // (a chrono event that reshapes the ground - a cutting, a tunnel's
                    // portal, a lake let down for a new road - saves the tile's terrain and
                    // water with its patch: read from the map's own files, the old ground
                    // filled the new tunnel and the old water stood over the new road,
                    // #923, #925)
                    if patch.has_terrain && omsi_cfg::vfs::is_file(&crate::scene::tile_companion(&p, ".terrain")) {
                        tile.terrain_from = Some(p.clone());
                    }
                    if patch.has_water && omsi_cfg::vfs::is_file(&crate::scene::tile_companion(&p, ".water")) {
                        tile.water_from = Some(p.clone());
                        tile.has_water = true;
                    }
                    let unmatched = tile.apply_chrono(&patch);
                    if unmatched > 0 {
                        log::debug!("chrono {}: {unmatched} selections name nothing in the tile", p.display());
                    }
                }
                Err(e) => log::warn!("{e}"),
            }
        }
        // (after the chrono patches: they are written in the tile's own measure)
        if let Some((_, ty)) = omsi_map::tile_index_of(&name.to_string_lossy()) {
            tile.fit_to_world_grid(ty);
        }
    }
    Some(tile)
}

/// The `.terrain` file of a tile read by [`read_tile`] from `path`: an active chrono
/// patch's own where it brings one (see `Tile::terrain_from`).
pub fn terrain_file(tile: &Tile, path: &Path) -> PathBuf {
    crate::scene::tile_companion(tile.terrain_from.as_deref().unwrap_or(path), ".terrain")
}

/// The `.water` file of a tile read by [`read_tile`] from `path` (see `Tile::water_from`).
pub fn water_file(tile: &Tile, path: &Path) -> PathBuf {
    crate::scene::tile_companion(tile.water_from.as_deref().unwrap_or(path), ".water")
}

/// The transform of `[new_attachment]` point `a` in its parent's frame (x right, y forward,
/// z up). The operations act on the attached object in the order they are listed, like the
/// original's Direct3D matrices (`attach_rot_z 180` then `attach_trans` turns a traffic
/// light round and then hangs it at the end of the beam), and the angles have the sign
/// convention of the model's origin rotations.
pub fn attachment_matrix(a: &omsi_scenery::sco::Attachment) -> Mat4 {
    let mut m = Mat4::IDENTITY;
    for (op, v) in &a.ops {
        let step = match (op.as_str(), v.as_slice()) {
            ("attach_trans", [x, y, z, ..]) => Mat4::from_translation(Vec3::new(*x, *y, *z)),
            ("attach_rot_x", [r, ..]) => Mat4::from_rotation_x(-r.to_radians()),
            ("attach_rot_y", [r, ..]) => Mat4::from_rotation_y(-r.to_radians()),
            ("attach_rot_z", [r, ..]) => Mat4::from_rotation_z(-r.to_radians()),
            _ => continue,
        };
        m = step * m;
    }
    m
}

/// A world pose: position, and the rotation of the object's frame.
#[derive(Debug, Clone, Copy)]
pub struct Pose {
    pub pos: DVec3,
    pub rot: Mat4,
}

impl Pose {
    /// The pose of an object hanging on attachment point `attach` of this pose, turned by
    /// its own heading/pitch/bank `own`, as Omsi.exe puts it there (0x79d4c4..0x79d689): the
    /// point's place turned with the parent, and for the turn the D3DX quaternion product
    /// point x own x parent - the point's rotation, then the object's own (bank, pitch,
    /// heading), then the parent's, all taken as rotations of the world axes. For the
    /// usual turns about the vertical this is the plain hierarchy; with a tilt in more than
    /// one of them it is what the original shows.
    pub fn attached(&self, attach: &Mat4, own: [f64; 3]) -> Pose {
        let local = attach.transform_point3(Vec3::ZERO);
        let pos = self.pos + self.rot.transform_vector3(local).as_dvec3();
        let (_, r, _) = attach.to_scale_rotation_translation();
        let (_, parent, _) = self.rot.to_scale_rotation_translation();
        let rot = Mat4::from_quat((omsi_geometry::object_rotation_ypr(own).to_scale_rotation_translation().1 * r * parent).normalize());
        Pose { pos, rot }
    }

    /// Heading (degrees, clockwise from north) of the pose's forward axis.
    pub fn heading(&self) -> f64 {
        let f = self.rot.transform_vector3(Vec3::Y);
        (f.x as f64).atan2(f.y as f64).to_degrees().rem_euclid(360.0)
    }
}

/// One object of a spline attachment row.
#[derive(Debug, Clone, Copy)]
pub struct RowObject {
    /// Index of the object in its row.
    pub index: usize,
    pub pose: Pose,
}

/// How long the chain from the start of spline `from` is before `to` starts, following the
/// `next` links (and the direction flips where two splines meet end to end), and whether
/// `to` is then run backwards. None when `to` is not on the chain within `limit` metres.
pub fn chain_distance(index: &MapIndex, from: i64, to: i64, limit: f64) -> Option<(f64, bool)> {
    let mut cur = from;
    let mut forward = true;
    let mut acc = 0.0;
    let mut seen: Vec<i64> = Vec::new();
    loop {
        let s = index.splines.get(&cur)?;
        if cur == to && !seen.is_empty() {
            return Some((acc, !forward));
        }
        if seen.contains(&cur) || seen.len() > 500 || acc > limit {
            return None;
        }
        seen.push(cur);
        acc += s.length;
        let next_id = if forward { s.next } else { s.prev };
        let next = index.splines.get(&next_id)?;
        if next.prev == cur {
            forward = true;
        } else if next.next == cur {
            forward = false;
        }
        cur = next_id;
    }
}

/// How far the start of spline `id` lies from the start of its chain. OMSI stores this in the
/// last `[spline]` field in tile version 11 and newer; that value is authoritative because
/// `prev`/`next` links can be stale. Older tiles do not store it, so reconstruct it by walking
/// the links (flipping direction where two splines meet end to end).
pub fn chain_offset(index: &MapIndex, id: i64) -> f64 {
    index.splines.get(&id).map(|s| chain_offset_from(index, id, *s)).unwrap_or(0.0)
}

/// Start with this record, not a foreign spline sharing its id. After the first step
/// the legacy chain follows the same predecessor links and loop guards as before.
fn chain_offset_from(index: &MapIndex, id: i64, mut spline: IndexedSpline) -> f64 {
    if let Some(offset) = spline.map_chain_offset {
        return offset;
    }
    let mut cur = id;
    let mut forward = true;
    let mut acc = 0.0;
    let mut seen: Vec<i64> = vec![id];
    loop {
        let prev_id = if forward { spline.prev } else { spline.next };
        let Some(p) = index.splines.get(&prev_id) else { break };
        if seen.contains(&prev_id) || seen.len() > 500 {
            break;
        }
        if p.next == cur {
            forward = true;
        } else if p.prev == cur {
            forward = false;
        }
        acc += p.length;
        seen.push(prev_id);
        cur = prev_id;
        spline = *p;
    }
    acc
}

/// How far past the end of a chain an object still stands at its end: the editor's sums of
/// spline lengths drift by a little (Spandau has buffer stops 0.1 and 0.5 m past theirs).
const CHAIN_END_TOLERANCE: f64 = 1.0;

/// Where a row record's objects begin on its own spline.
#[derive(Debug, Clone, Copy)]
struct RowStart {
    /// Distance of the first object from the start of the spline (the objects before the
    /// start are skipped).
    s: f64,
    /// Index of that object in the row.
    j: usize,
    /// The spline runs against the row.
    backwards: bool,
    /// The row's start and this spline's start, as distances along the chain from the start
    /// of the master's spline.
    d0: f64,
    acc: f64,
}

/// The first object of a row starting at `d0` that lies after `acc` along the chain (one
/// standing right on a joint belongs to the spline before): (its distance past `acc`, its
/// index).
fn first_after(d0: f64, acc: f64, interval: f64) -> Option<(f64, usize)> {
    if interval > 0.0 {
        let j = ((acc - d0) / interval + 1e-6).ceil().max(0.0) as usize;
        Some((d0 + j as f64 * interval - acc, j))
    } else if d0 >= acc - 1e-6 {
        Some((d0 - acc, 0))
    } else {
        None
    }
}

/// Where the objects of row record `att` begin on `spline`, its own spline.
fn row_start(att: &SplineAttachment, spline: &MapSpline, index: Option<&MapIndex>) -> Option<RowStart> {
    let interval = att.interval.max(0.0);
    let d = att.offset[2];
    let Some((master_tile, first)) = att.repeater else {
        // the start distance counts from the start of the chain
        // Separate towns in a merged map can reuse spline ids. The record's authored
        // offset or legacy links belong to this record; a global id may name another.
        let offset = spline.map_chain_offset
            .or_else(|| index.map(|ix| chain_offset_from(ix, spline.id, IndexedSpline::from_map(spline))))
            .unwrap_or(0.0);
        let d0 = d - offset;
        return Some(RowStart { s: d0, j: 0, backwards: false, d0, acc: 0.0 });
    };
    let master = index.and_then(|ix| ix.masters.get(&(master_tile, att.id)).copied());
    if let (Some(ix), Some((master_spline, d0))) = (index, master) {
        let limit = d0.max(0.0) + att.range.max(0.0) + spline.length.max(0.0) + 1000.0;
        let Some((acc, backwards)) = chain_distance(ix, master_spline, spline.id, limit) else {
            if interval > 0.0 {
                log::debug!("spline attachment {} ({}): spline {} not on the chain of {master_spline}", att.id, att.file, spline.id);
            }
            return None;
        };
        return first_after(d0, acc, interval).map(|(s, j)| RowStart { s, j, backwards, d0, acc });
    }
    if interval > 0.0 && index.is_some() {
        // the master is not in the map (a broken chain in a mod map): keep the row's
        // spacing with its own count, starting at the interval's phase
        let s = d % interval;
        return Some(RowStart { s, j: first, backwards: false, d0: s - first as f64 * interval, acc: 0.0 });
    }
    None
}

/// The objects of row record `att` on `spline` from `start` on, while `j * interval <= range`.
fn place_on(att: &SplineAttachment, spline: &MapSpline, origin: DVec2, index: Option<&MapIndex>, start: RowStart) -> Vec<RowObject> {
    let curve = SplineCurve { half_cant_width: omsi_geometry::half_cant_width_of(&spline.file), ..SplineCurve::from_map(spline, origin) };
    let len = spline.length.max(0.0);
    let interval = att.interval.max(0.0);
    let range = att.range.max(0.0);
    let (x, h) = (att.offset[0], att.offset[1]);
    let RowStart { mut s, mut j, backwards, .. } = start;
    // where the chain ends with this spline (in the row's direction), an object a little
    // past the end stands at the end
    let far = if backwards { spline.prev_id } else { spline.next_id };
    let chain_ends = index.map(|ix| !ix.splines.contains_key(&far)).unwrap_or(far == 0);
    let end = len + if chain_ends { CHAIN_END_TOLERANCE } else { 1e-6 };
    let mut out = Vec::new();
    loop {
        if interval > 0.0 && j as f64 * interval > range + 1e-6 {
            break;
        }
        if s > end {
            break;
        }
        if s >= -1e-6 {
            let along = s.clamp(0.0, len);
            // a spline run against the row: the row's distance counts from its far end and
            // the row's right is the spline's left
            let (u, side, turn) = if backwards { (len - along, -x, 180.0) } else { (along, x, 0.0) };
            let pos = curve.offset_point(u, side, h);
            let pitch = if att.tilt { curve.slope_at(u).atan().to_degrees() } else { 0.0 };
            // The cant lifts only within the spline type's half cant width.
            let bank = if att.tilt && side.abs() < curve.half_cant_width {
                (curve.cant_at(u) / 100.0).atan().to_degrees()
            } else { 0.0 };
            let mut own = omsi_geometry::map_rotation(att.rot);
            own[0] += turn;
            // Tangential objects turn in the spline's inclined frame. Adding the
            // spline's pitch/bank to the object's Euler angles instead tilts a
            // sideways railing across the road, and a reversed object downhill.
            // The half turn of a backwards chain belongs to that same local frame.
            let rot = omsi_geometry::object_rotation([curve.heading_at(u), pitch, bank])
                * omsi_geometry::object_rotation(own);
            out.push(RowObject { index: j, pose: Pose { pos, rot } });
        }
        if interval <= 0.0 {
            break;
        }
        s += interval;
        j += 1;
        if out.len() > 4096 {
            log::warn!("spline attachment {} ({}): more than 4096 objects on one spline, row cut short", att.id, att.file);
            break;
        }
    }
    out
}

/// The objects of row record `att` of a tile whose splines are `splines`, whose curves start
/// at `origin` (the tile origin). Object `j` of a row lies `d + j * interval` along the chain
/// from the chain's start (see [`chain_offset`]) while `j * interval <= range`: on the
/// record's own spline and on the splines after it in the chain as long as they are in the
/// same tile - where the
/// chain enters another tile, a repeater there carries the row on (every one of the 169
/// repeaters of Berlin-Spandau with objects lies on the first spline of a new tile, and 413
/// later splines in the record's own tile have none). (index into `splines`, object).
pub fn tile_row_objects(att: &SplineAttachment, splines: &[MapSpline], origin: DVec2, index: Option<&MapIndex>) -> Vec<(usize, RowObject)> {
    let si = att.spline_index.max(0) as usize;
    let Some(own) = splines.get(si).filter(|s| !s.deleted) else { return Vec::new() };
    let Some(start) = row_start(att, own, index) else { return Vec::new() };
    let interval = att.interval.max(0.0);
    let last = start.d0 + if interval > 0.0 { (att.range.max(0.0) / interval + 1e-6).floor() * interval } else { 0.0 };
    let mut out: Vec<(usize, RowObject)> = place_on(att, own, origin, index, start).into_iter().map(|o| (si, o)).collect();
    let (mut cur, mut acc, mut backwards) = (si, start.acc, start.backwards);
    let mut seen = vec![si];
    loop {
        acc += splines[cur].length.max(0.0);
        if last <= acc + 1e-6 {
            break;
        }
        let c = &splines[cur];
        let next_id = if backwards { c.prev_id } else { c.next_id };
        let Some(ni) = splines.iter().position(|s| s.id == next_id && !s.deleted) else { break };
        if seen.contains(&ni) || seen.len() > 500 {
            break;
        }
        seen.push(ni);
        let next = &splines[ni];
        if next.prev_id == c.id {
            backwards = false;
        } else if next.next_id == c.id {
            backwards = true;
        }
        let Some((s, j)) = first_after(start.d0, acc, interval) else { break };
        out.extend(place_on(att, next, origin, index, RowStart { s, j, backwards, d0: start.d0, acc }).into_iter().map(|o| (ni, o)));
        cur = ni;
    }
    out
}

/// A finished batch from the worker: the prepared tiles, their statistics and how long the
/// preparation took.
/// A finished batch: the tiles asked for, what was made of them (a tile that could not be
/// made is missing), statistics and seconds.
type Batch = (Vec<(i32, i32)>, Vec<crate::scene::Prepared>, crate::scene::LoadStats, f64);

/// The threads tiles are prepared on while the game runs: a pool of their own, a third of
/// the cores, so that the frame's parallel work (culling, the AI scripts) never waits for a
/// tile to finish on the shared pool - and at a lower priority, so that they do not take
/// the cores those workers need either (see `threads`).
fn loader_pool() -> &'static rayon::ThreadPool {
    static POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let n = (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) / 3).max(2);
        rayon::ThreadPoolBuilder::new().num_threads(n).thread_name(|i| format!("tile loader {i}")).start_handler(|_| crate::threads::lower_thread_priority()).build().expect("tile loader pool")
    })
}

/// Candidates whose tile rectangles may touch `radius` around a center. The exact
/// circular distance check follows in `missing`.
fn tile_candidates(
    lookup: &hashbrown::HashMap<(i32, i32), Vec<usize>>,
    tile_count: usize,
    centers: &[DVec3],
    radius: f64,
) -> Vec<usize> {
    let ts = tile_size();
    let mut candidates: Vec<usize> = Vec::new();
    for c in centers {
        let min_x = ((c.x - radius) / ts).floor() as i32 - 1;
        let max_x = ((c.x + radius) / ts).floor() as i32;
        let min_y = ((c.y - radius) / ts).floor() as i32 - 1;
        let max_y = ((c.y + radius) / ts).floor() as i32;
        let cells = (max_x as i64 - min_x as i64 + 1) * (max_y as i64 - min_y as i64 + 1);
        if cells > tile_count as i64 * 2 {
            candidates.extend(0..tile_count);
            break;
        }
        for x in min_x..=max_x {
            for y in min_y..=max_y {
                if let Some(indices) = lookup.get(&(x, y)) {
                    candidates.extend_from_slice(indices);
                }
            }
        }
    }
    candidates.sort_unstable();
    candidates.dedup();
    candidates
}

fn initial_stall_due(
    stalled_for: std::time::Duration,
    since_last_stall: Option<std::time::Duration>,
) -> bool {
    stalled_for >= std::time::Duration::from_secs(15)
        && match since_last_stall {
            Some(age) => age >= std::time::Duration::from_secs(30),
            None => true,
        }
}

/// Loads the tiles around a few points as they move (the camera, and the player's bus,
/// which must not lose the ground under it when the free camera flies off), like OMSI's
/// tile streaming: the tiles within `load_radius` of any of them are read, tessellated and
/// placed on a worker thread (a few at a time, nearest first), put on the GPU a little each
/// frame, and the tiles beyond `unload_radius` of all of them are taken away again, so the
/// memory a map needs is bounded by the view distance and not by the size of the map.
pub struct Streamer {
    world: std::sync::Arc<crate::scene::World>,
    tiles: Vec<(i32, i32, PathBuf)>,
    /// Indices into `tiles`, including duplicate coordinates, in map-file order.
    tile_lookup: hashbrown::HashMap<(i32, i32), Vec<usize>>,
    pub load_radius: f64,
    pub unload_radius: f64,
    tx: std::sync::mpsc::Sender<Batch>,
    rx: std::sync::mpsc::Receiver<Batch>,
    inflight: bool,
    queue: std::collections::VecDeque<crate::scene::PendingUpload>,
    /// Tiles in flight or waiting for upload.
    requested: hashbrown::HashSet<(i32, i32)>,
    /// Tiles that could not be made (a damaged file): not asked for again.
    failed: hashbrown::HashSet<(i32, i32)>,
    /// The first area around the start: (tiles, of which uploaded).
    initial: Option<(hashbrown::HashSet<(i32, i32)>, usize)>,
    pub stats: crate::scene::LoadStats,
    pub loaded_total: usize,
    pub unloaded_total: usize,
    /// Seconds the worker spent preparing, and the longest upload work of one frame.
    pub prepare_secs: f64,
    pub worst_upload_ms: f64,
    /// The longest frame share of streaming after the first area (upload, unload and the
    /// lists), in ms, and how often it took more than 16 ms.
    pub worst_frame_ms: f64,
    pub slow_frames: usize,
    started: std::time::Instant,
    last_summary: std::time::Instant,
    /// First-area diagnostics are deliberately tiny: one progress line per tile and a
    /// rate-limited stall line. They exist only while the loading screen is up.
    initial_last_done: usize,
    initial_last_progress: std::time::Instant,
    initial_last_stall_log: Option<std::time::Instant>,
    /// The worker batch currently being prepared, for a useful stall message.
    inflight_batch: Option<(Vec<(i32, i32)>, std::time::Instant)>,
    /// One initial tile may take several frames to upload/place; time it across those frames.
    initial_upload: Option<((i32, i32), std::time::Instant)>,
    /// Tiles loaded and unloaded when the heap's free pages were last given back, and when.
    relieved_at: (usize, usize, std::time::Instant),
}

impl Streamer {
    /// Start streaming around `centers`: the tiles within `initial_radius` of them form the
    /// first area, which is what the loading screen waits for.
    pub fn new(world: std::sync::Arc<crate::scene::World>, centers: &[DVec3], load_radius: f64, initial_radius: f64) -> Streamer {
        let tiles = world.select_tiles(None, None);
        let mut tile_lookup: hashbrown::HashMap<(i32, i32), Vec<usize>> = hashbrown::HashMap::new();
        for (i, t) in tiles.iter().enumerate() {
            tile_lookup.entry((t.0, t.1)).or_default().push(i);
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let mut s = Streamer {
            world,
            tiles,
            tile_lookup,
            load_radius,
            unload_radius: load_radius + tile_size() * 1.5,
            tx,
            rx,
            inflight: false,
            queue: Default::default(),
            requested: Default::default(),
            failed: Default::default(),
            initial: None,
            stats: Default::default(),
            loaded_total: 0,
            unloaded_total: 0,
            prepare_secs: 0.0,
            worst_upload_ms: 0.0,
            worst_frame_ms: 0.0,
            slow_frames: 0,
            started: std::time::Instant::now(),
            last_summary: std::time::Instant::now(),
            initial_last_done: 0,
            initial_last_progress: std::time::Instant::now(),
            initial_last_stall_log: None,
            inflight_batch: None,
            initial_upload: None,
            relieved_at: (0, 0, std::time::Instant::now()),
        };
        let first: hashbrown::HashSet<(i32, i32)> = s.tiles.iter().filter(|t| Self::nearest(centers, t.0, t.1) <= initial_radius.min(load_radius)).map(|t| (t.0, t.1)).collect();
        log::info!("tile streaming: {} tiles in the map, load radius {:.0} m around {} points ({} tiles now), first area {} tiles", s.tiles.len(), load_radius, centers.len(), s.tiles.iter().filter(|t| Self::nearest(centers, t.0, t.1) <= load_radius).count(), first.len());
        s.initial = Some((first, 0));
        s
    }

    /// Distance from `p` to the nearest point of tile (tx, ty).
    pub fn distance(p: DVec3, tx: i32, ty: i32) -> f64 {
        let ts = tile_size();
        let (x0, y0) = (tx as f64 * ts, ty as f64 * ts);
        let dx = (x0 - p.x).max(0.0).max(p.x - (x0 + ts));
        let dy = (y0 - p.y).max(0.0).max(p.y - (y0 + ts));
        (dx * dx + dy * dy).sqrt()
    }

    /// Distance from the nearest of `centers` to tile (tx, ty).
    pub fn nearest(centers: &[DVec3], tx: i32, ty: i32) -> f64 {
        centers.iter().map(|c| Self::distance(*c, tx, ty)).fold(f64::INFINITY, f64::min)
    }

    /// Read tiles `keys` again (a chrono scenario changed them, the season's textures
    /// changed): those loaded go and come back with the next batches. `None`: every loaded
    /// tile.
    pub fn reload(&mut self, renderer: &omsi_render::Renderer, scene: &mut omsi_render::Scene, keys: Option<&[(i32, i32)]>, audio: Option<&omsi_audio::AudioEngine>) {
        let keys: Vec<(i32, i32)> = match keys {
            Some(k) => k.to_vec(),
            None => {
                self.world.forget_all_staged();
                self.world.loaded_tiles()
            }
        };
        let mut freed = false;
        for k in &keys {
            self.failed.remove(k);
            if !self.requested.contains(k) {
                freed |= self.world.unload_tile(renderer, scene, *k, audio);
            }
        }
        if freed {
            self.world.trim_object_types();
        }
        self.world.refresh_tile_lists();
        log::info!("tile streaming: {} tiles to be read again", keys.len());
    }

    /// (uploaded, total) of the first area while it is still loading.
    pub fn initial_progress(&self) -> Option<(usize, usize)> {
        self.initial.as_ref().filter(|(set, done)| *done < set.len()).map(|(set, done)| (*done, set.len()))
    }

    fn missing(&self, centers: &[DVec3]) -> Vec<(f64, (i32, i32, PathBuf))> {
        let loaded: hashbrown::HashSet<(i32, i32)> = self.world.loaded_tiles().into_iter().collect();
        let mut out: Vec<(f64, (i32, i32, PathBuf))> = tile_candidates(&self.tile_lookup, self.tiles.len(), centers, self.load_radius)
            .into_iter()
            .map(|i| &self.tiles[i])
            .filter(|t| !loaded.contains(&(t.0, t.1)) && !self.requested.contains(&(t.0, t.1)) && !self.failed.contains(&(t.0, t.1)))
            .map(|t| (Self::nearest(centers, t.0, t.1), t.clone()))
            .filter(|(d, _)| *d <= self.load_radius)
            .collect();
        out.sort_by(|a, b| a.0.total_cmp(&b.0));
        out
    }

    /// One frame of streaming around `centers`: take in what the worker finished, upload and
    /// place for about `budget` (always a little), unload what is far from all of them (for
    /// about half a budget more, at least one tile), and start the next batch. Returns true
    /// when the loaded tiles changed (the caller then refreshes whatever copies world data).
    pub fn update(&mut self, renderer: &omsi_render::Renderer, scene: &mut omsi_render::Scene, centers: &[DVec3], budget: std::time::Duration, audio: Option<&omsi_audio::AudioEngine>) -> bool {
        let mut changed = false;
        while let Ok((asked, prepared, stats, secs)) = self.rx.try_recv() {
            self.inflight = false;
            if let Some((keys, started)) = self.inflight_batch.take() {
                let elapsed = started.elapsed().as_secs_f64();
                if elapsed >= 2.0 {
                    log::warn!("tile loading: first-area worker batch {:?} returned after {:.2} s", keys, elapsed);
                } else {
                    log::info!("tile loading: first-area worker batch {:?} returned in {:.2} s", keys, elapsed);
                }
            }
            // a tile the worker could not make is let go (else it stayed "requested" for
            // ever: never retried, never unloaded, and the loading screen waited for it)
            let made: hashbrown::HashSet<(i32, i32)> = prepared.iter().map(|p| (p.tx, p.ty)).collect();
            for k in asked.into_iter().filter(|k| !made.contains(k)) {
                log::warn!("tile streaming: tile {},{} could not be loaded; left out", k.0, k.1);
                self.requested.remove(&k);
                self.failed.insert(k);
                if let Some((_, done)) = self.initial.as_mut().filter(|(set, _)| set.contains(&k)) {
                    *done += 1;
                    changed = true;
                }
            }
            self.prepare_secs += secs;
            self.stats.add_prepared(&stats);
            self.queue.extend(prepared.into_iter().map(|p| self.world.begin_upload(p)));
        }
        let t0 = std::time::Instant::now();
        let mut uploaded = 0usize;
        let mut freed_types = false;
        let deadline = t0 + budget;
        while let Some(mut p) = self.queue.pop_front() {
            let key = p.key();
            let initial = self.initial.as_ref().map(|(set, _)| set.contains(&key)).unwrap_or(false);
            if initial && self.initial_upload.as_ref().map(|(k, _)| *k) != Some(key) {
                self.initial_upload = Some((key, std::time::Instant::now()));
            }
            if !initial && Self::nearest(centers, key.0, key.1) > self.unload_radius {
                // gone out of range while it was being prepared: what it already holds on
                // the GPU goes back with it
                self.requested.remove(&key);
                self.world.abandon_upload(renderer, scene, p, audio);
                changed = true;
                continue;
            }
            let t = std::time::Instant::now();
            // its new textures and object types first, then the tile itself, a little a frame
            let ready = self.world.upload_step(renderer, scene, &mut p, Some(deadline)) && self.world.place_step(renderer, scene, &mut p, Some(deadline));
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            if self.initial.as_ref().map(|(_, done)| *done > 0).unwrap_or(true) {
                self.worst_upload_ms = self.worst_upload_ms.max(ms);
            }
            if !ready {
                self.queue.push_front(p);
                break;
            }
            self.requested.remove(&key);
            if initial {
                let secs = self
                    .initial_upload
                    .take()
                    .filter(|(k, _)| *k == key)
                    .map(|(_, started)| started.elapsed().as_secs_f64())
                    .unwrap_or(ms / 1000.0);
                if secs >= 2.0 {
                    log::warn!("tile loading: slow upload/place tile {},{} took {:.2} s", key.0, key.1, secs);
                } else {
                    log::info!("tile loading: uploaded/placed tile {},{} in {:.2} s", key.0, key.1, secs);
                }
            }
            let mut stats = crate::scene::LoadStats::default();
            self.world.commit_upload(p, &mut stats);
            self.stats.objects += stats.objects;
            self.stats.trees += stats.trees;
            self.stats.splines += stats.splines;
            self.stats.tiles += 1;
            self.loaded_total += 1;
            if let Some((_, done)) = self.initial.as_mut().filter(|(set, _)| set.contains(&key)) {
                *done += 1;
            }
            changed = true;
            uploaded += 1;
            if t0.elapsed() >= budget {
                break;
            }
        }
        let t_upload = t0.elapsed();
        let mut unloaded = 0usize;
        if self.initial.as_ref().map(|(set, done)| *done >= set.len()).unwrap_or(false) {
            let (set, _) = self.initial.take().unwrap();
            self.inflight_batch = None;
            self.initial_upload = None;
            log::info!("tile streaming: first area of {} tiles loaded in {:.2} s", set.len(), self.started.elapsed().as_secs_f64());
        }
        // Far tiles go (never the ones on their way in), the farthest first and only as many
        // as fit in the budget (at least one a frame): a camera jump across a big map had
        // 42 tiles taken away in one frame of 400 ms. Unloading gets its own share of the
        // budget, so that a busy upload queue cannot hold the memory of the old area.
        if self.initial.is_none() {
            let now = std::time::Instant::now();
            let unload_deadline = deadline.max(now + budget / 2);
            let mut far: Vec<(f64, (i32, i32))> = self.world.loaded_tiles().into_iter().filter(|k| !self.requested.contains(k)).map(|k| (Self::nearest(centers, k.0, k.1), k)).filter(|(d, _)| *d > self.unload_radius).collect();
            far.sort_by(|a, b| b.0.total_cmp(&a.0));
            for (_, key) in far {
                if unloaded > 0 && std::time::Instant::now() >= unload_deadline {
                    break;
                }
                freed_types |= self.world.unload_tile(renderer, scene, key, audio);
                self.unloaded_total += 1;
                unloaded += 1;
                changed = true;
            }
        }
        if freed_types {
            self.world.trim_object_types();
        }
        if unloaded > 0 {
            // the tiles only they depended on go too
            self.world.trim_staged(&self.requested);
        }
        let t_unload = t0.elapsed();
        if changed {
            self.world.refresh_tile_lists();
        }
        // Loading and unloading tiles leaves the allocator with pages it keeps for later: they
        // count as the game's memory until they are handed back (half a gigabyte on
        // Ahlheim). At most every few seconds, on a thread of its own.
        if (self.loaded_total, self.unloaded_total) != (self.relieved_at.0, self.relieved_at.1) && self.relieved_at.2.elapsed().as_secs_f32() >= 4.0 && self.queue.is_empty() && !self.inflight {
            self.relieved_at = (self.loaded_total, self.unloaded_total, std::time::Instant::now());
            self.world.compact_slots(renderer, scene);
            crate::release_free_memory();
        }
        if self.last_summary.elapsed().as_secs_f32() >= 10.0 && omsi_cfg::env::var_os("OMSI_PROFILE").is_some() {
            self.last_summary = std::time::Instant::now();
            let at: Vec<String> = centers.iter().map(|c| format!("({:.0}, {:.0})", c.x, c.y)).collect();
            log::info!("tile streaming at {}: {} loaded / {} unloaded so far; {}", at.join(" "), self.loaded_total, self.unloaded_total, self.world.gpu_summary(scene));
            if omsi_cfg::env::var_os("OMSI_PROFILE").is_some() {
                log::info!("  {}", self.world.cpu_summary());
            }
        }
        let total = t0.elapsed();
        if self.initial.is_none() && total.as_secs_f64() * 1000.0 > 16.0 {
            self.slow_frames += 1;
            if omsi_cfg::env::var_os("OMSI_PROFILE").is_some() {
                log::info!("tile streaming: {:.0} ms this frame ({uploaded} uploaded in {:.0} ms, {unloaded} unloaded in {:.0} ms, lists {:.0} ms)", total.as_secs_f64() * 1000.0, t_upload.as_secs_f64() * 1000.0, (t_unload - t_upload).as_secs_f64() * 1000.0, (total - t_unload).as_secs_f64() * 1000.0);
            }
        }
        if self.initial.is_none() {
            self.worst_frame_ms = self.worst_frame_ms.max(total.as_secs_f64() * 1000.0);
        }

        // While the loading screen is up, make a stuck worker visible without writing once
        // per frame. Progress is at most one line per initial tile; a genuine stall is one
        // warning after 15 s and then at most one every 30 s.
        if let Some((done, total_initial)) = self.initial.as_ref().map(|(set, done)| (*done, set.len())) {
            let now = std::time::Instant::now();
            if done != self.initial_last_done {
                self.initial_last_done = done;
                self.initial_last_progress = now;
                self.initial_last_stall_log = None;
                log::info!(
                    "tile loading: first area progress {}/{} after {:.1} s",
                    done,
                    total_initial,
                    self.started.elapsed().as_secs_f64()
                );
            } else {
                let stalled_for = now.duration_since(self.initial_last_progress);
                let since_last = self.initial_last_stall_log.map(|t| now.duration_since(t));
                if initial_stall_due(stalled_for, since_last) {
                    let worker = self
                        .inflight_batch
                        .as_ref()
                        .map(|(keys, t)| format!("worker {:?} running {:.1} s", keys, t.elapsed().as_secs_f64()))
                        .unwrap_or_else(|| "worker idle".to_string());
                    let upload = self
                        .initial_upload
                        .as_ref()
                        .map(|(key, t)| format!("upload/place {},{} running {:.1} s", key.0, key.1, t.elapsed().as_secs_f64()))
                        .unwrap_or_else(|| format!("{} prepared tile(s) waiting for upload", self.queue.len()));
                    log::warn!(
                        "tile loading: first area stalled at {}/{} for {:.1} s; {}; {}",
                        done,
                        total_initial,
                        stalled_for.as_secs_f64(),
                        worker,
                        upload
                    );
                    self.initial_last_stall_log = Some(now);
                }
            }
        }

        if !self.inflight {
            let missing = self.missing(centers);
            if !missing.is_empty() {
                // the first area in bigger bites (the loading screen shows the progress),
                // then a few tiles at a time
                let n = if self.initial.is_some() { 6 } else { 3 };
                let batch: Vec<(i32, i32, PathBuf)> = missing.into_iter().take(n).map(|(_, t)| t).collect();
                let keys: Vec<(i32, i32)> = batch.iter().map(|t| (t.0, t.1)).collect();
                self.requested.extend(keys.iter().copied());
                self.inflight = true;
                let world = self.world.clone();
                let tx = self.tx.clone();
                // the first area gets every core; later tiles only the loader's own threads
                let first = self.initial.is_some();
                if first {
                    log::info!("tile loading: first-area worker batch {:?} started", keys);
                    self.inflight_batch = Some((keys.clone(), std::time::Instant::now()));
                }
                let spawned = std::thread::Builder::new().name("tile loader".into()).spawn(move || {
                    let t = std::time::Instant::now();
                    // (a panic on a damaged file must not end the streaming: the batch comes
                    // back empty and its tiles are let go)
                    let made = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| if first { world.prepare_tiles_initial(&batch) } else { loader_pool().install(|| world.prepare_tiles(&batch)) }));
                    let (prepared, stats) = made.unwrap_or_else(|_| {
                        log::error!("tile streaming: loading tiles {:?} failed", batch.iter().map(|t| (t.0, t.1)).collect::<Vec<_>>());
                        Default::default()
                    });
                    let _ = tx.send((batch.iter().map(|t| (t.0, t.1)).collect(), prepared, stats, t.elapsed().as_secs_f64()));
                });
                if let Err(e) = spawned {
                    log::warn!("tile loader thread: {e}");
                    self.inflight = false;
                    self.inflight_batch = None;
                    for k in keys {
                        self.requested.remove(&k);
                    }
                }
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_area_stall_log_is_delayed_and_rate_limited() {
        use std::time::Duration;

        assert!(!initial_stall_due(Duration::from_secs(14), None));
        assert!(initial_stall_due(Duration::from_secs(15), None));
        assert!(!initial_stall_due(
            Duration::from_secs(60),
            Some(Duration::from_secs(29))
        ));
        assert!(initial_stall_due(
            Duration::from_secs(60),
            Some(Duration::from_secs(30))
        ));
    }

    fn synthetic_spline_stop(version: i32, stop: i64, offset: f64, distance: f64, spline_index: usize) -> String {
        let links = if version >= 11 { "0\n0\n" } else { "-1\n" };
        let chain = if version >= 11 { format!("0\n0\n{offset}\n") } else { String::new() };
        let turn = if version >= 12 { "0\n0\n0\n10\n0\n0\n" } else { "0\n10\n0\n" };
        format!("[version]\n{version}\n\n[spline]\n0\nsynthetic.sli\n10\n{links}0\n0\n0\n0\n50\n0\n0\n0\n0\n0\n{chain}\n[splineAttachement]\n0\nsynthetic-stop.sco\n{stop}\n{spline_index}\n0\n0\n{distance}\n{turn}0\n")
    }

    #[test]
    fn cold_child_stop_keeps_its_spline_attached_parent_and_metadata() {
        let dir = std::env::temp_dir().join(format!("omsi-index-child-spline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tile_3_-2.map");
        // The parent's 625 m distance includes 600 m before its own 50 m segment.
        // Its child can be indexed only after that parent's row pose is resolved.
        let contents = synthetic_spline_stop(14, 100, 600.0, 625.0, 0)
            + "\n[attachObj]\n0\nsynthetic-child.sco\n200\n100\n0\n0\n30\n0\n0\n6\nChild stop\n7\n2\n11\n22.5\n1\n";
        std::fs::write(&path, contents).unwrap();
        let index = MapIndex::build(&[(0, 3, -2, path)], &[], &dir);
        let expected = DVec3::new(3.0 * tile_size(), -2.0 * tile_size() + 25.0, 0.0);
        for id in [100, 200] {
            let (tile, pos, _) = index.objects.get(&id).expect("both parent and child must be indexed cold");
            assert_eq!(*tile, (3, -2));
            assert!((*pos - expected).length() < 1e-8);
        }
        assert_eq!(index.objects[&200].2, [30.0, 0.0, 0.0]);
        assert_eq!(index.stop_enter[&200], (7.0, 2.0));
        assert_eq!(index.stop_weights[&200], 11.0);
        assert_eq!(index.stop_length[&200], 22.5);
        assert_eq!(index.stop_side[&200], 1.0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn authored_offsets_keep_stops_on_tiles_with_duplicate_spline_ids() {
        let dir = std::env::temp_dir().join(format!("omsi-index-duplicate-spline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (first, second) = (dir.join("tile_0_0.map"), dir.join("tile_2_-1.map"));
        for version in [10, 14] {
            // A legacy root has no authored offset; a modern root explicitly owns zero.
            // Neither must inherit the unrelated town's offset from the same spline id.
            std::fs::write(&first, synthetic_spline_stop(version, 100, 0.0, 25.0, 0)).unwrap();
            std::fs::write(&second, synthetic_spline_stop(14, 200, 600.0, 625.0, 0)).unwrap();
            let index = MapIndex::build(&[(0, 0, 0, first.clone()), (1, 2, -1, second.clone())], &[], &dir);
            assert_eq!(index.splines[&10].map_chain_offset, Some(600.0));
            for (tile_index, stop, x, y, path) in [(0, 100, 0, 0, &first), (1, 200, 2, -1, &second)] {
                let origin = DVec2::new(x as f64 * tile_size(), y as f64 * tile_size());
                let pos = index.objects.get(&stop).expect("both towns must keep their stop").1;
                assert!((pos - origin.extend(0.0) - DVec3::new(0.0, 25.0, 0.0)).length() < 1e-8);
                assert_eq!(index.masters[&(tile_index, stop)].1, 25.0);
                let tile = read_tile(path, &[]).unwrap();
                let placed = tile_row_objects(&tile.spline_attachments[0], &tile.splines, origin, Some(&index));
                assert_eq!(placed.len(), 1);
                assert!((placed[0].1.pose.pos - pos).length() < 1e-8, "streamed placement also uses the local record's offset");
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn legacy_stops_survive_modern_chrono_with_reused_spline_id() {
        let dir = std::env::temp_dir().join(format!("omsi-index-chrono-spline-{}", std::process::id()));
        let chrono = dir.join("Chrono/addition");
        std::fs::create_dir_all(&chrono).unwrap();
        let path = dir.join("tile_0_0.map");
        std::fs::write(&path, synthetic_spline_stop(10, 100, 0.0, 25.0, 0)).unwrap();
        // Chrono attachment indices count in the combined spline list.
        std::fs::write(chrono.join("tile_0_0.map"), synthetic_spline_stop(14, 200, 600.0, 625.0, 1)).unwrap();
        let index = MapIndex::build(&[(0, 0, 0, path.clone())], &[chrono.clone()], &dir);
        let tile = read_tile(&path, &[chrono]).unwrap();
        assert_eq!(tile.splines[0].map_chain_offset, None);
        assert_eq!(tile.splines[1].map_chain_offset, Some(600.0));
        for attachment in &tile.spline_attachments {
            let pos = index.objects.get(&attachment.id).expect("chrono must preserve both stops").1;
            assert!((pos - DVec3::new(0.0, 25.0, 0.0)).length() < 1e-8);
            assert_eq!(index.masters[&(0, attachment.id)].1, 25.0);
            let placed = tile_row_objects(attachment, &tile.splines, DVec2::ZERO, Some(&index));
            assert_eq!(placed.len(), 1);
            assert!((placed[0].1.pose.pos - pos).length() < 1e-8);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cold_index_resolves_chained_spline_stops() {
        let dir = std::env::temp_dir().join(format!("omsi-index-stop-chain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for version in [10, 14] {
            // Entirely synthetic: the stop lies 25 m into the second segment, but its
            // authored distance (625 m) includes the 600 m before that segment.
            let spline = |id: i64, y: f64, length: f64, offset: f64| {
                let links = if version >= 11 {
                    // Stale predecessor links must not override the authored offset.
                    format!("0\n{}\n", if id == 1 { 2 } else { 0 })
                } else {
                    format!("{}\n", if id == 1 { -1 } else { 0 })
                };
                let chain = if version >= 11 { format!("0\n0\n{offset}\n") } else { String::new() };
                format!("[spline]\n0\nsynthetic.sli\n{id}\n{links}0\n0\n{y}\n0\n{length}\n0\n0\n0\n0\n0\n{chain}\n")
            };
            let turn = if version >= 12 { "180\n0\n0\n10\n0\n1\n" } else { "180\n10\n0\n" };
            let contents = format!("[version]\n{version}\n\n{}{}[splineAttachement]\n0\nsynthetic-stop.sco\n100\n1\n-3\n2\n625\n{turn}0\n",
                spline(1, 0.0, 600.0, 0.0), spline(2, 600.0, 50.0, 600.0));
            let path = dir.join(format!("tile_3_-2_v{version}.map"));
            std::fs::write(&path, contents).unwrap();
            let index = MapIndex::build(&[(0, 3, -2, path.clone())], &[], &dir);
            let (_, pos, rot) = index.objects.get(&100).expect("the cold index must know the stop");
            assert!((*pos - DVec3::new(3.0 * tile_size() - 3.0, -2.0 * tile_size() + 625.0, 2.0)).length() < 1e-8);
            let tile = read_tile(&path, &[]).unwrap();
            let placed = tile_row_objects(&tile.spline_attachments[0], &tile.splines,
                DVec2::new(3.0 * tile_size(), -2.0 * tile_size()), Some(&index));
            assert_eq!(placed.len(), 1);
            assert!((*pos - placed[0].1.pose.pos).length() < 1e-8, "cold and streamed placement must agree");
            assert!((rot[0] - placed[0].1.pose.heading()).abs() < 1e-6);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An active chrono patch with `[terrain]`/`[water]` and their files beside it gives the
    /// tile its ground and water (#923, #925); one without them leaves the map's own.
    #[test]
    fn a_chrono_patch_with_its_own_terrain_and_water_replaces_the_tiles() {
        let dir = std::env::temp_dir().join(format!("omsi-chrono-terrain-{}", std::process::id()));
        let (base, c1, c2) = (dir.join("map"), dir.join("map/Chrono/a"), dir.join("map/Chrono/b"));
        for d in [&base, &c1, &c2] {
            std::fs::create_dir_all(d).unwrap();
        }
        let name = "tile_0_0.map";
        std::fs::write(base.join(name), "[version]\n14\n\n[terrain]\n0\n\n[water]\n0\n").unwrap();
        std::fs::write(base.join(format!("{name}.terrain")), b"x").unwrap();
        std::fs::write(base.join(format!("{name}.water")), b"x").unwrap();
        // a: reshapes the ground and the water; b (later): only objects
        std::fs::write(c1.join(name), "[version]\n14\n\n[terrain]\n0\n\n[water]\n0\n").unwrap();
        std::fs::write(c1.join(format!("{name}.terrain")), b"x").unwrap();
        std::fs::write(c1.join(format!("{name}.water")), b"x").unwrap();
        std::fs::write(c2.join(name), "[version]\n14\n").unwrap();
        let path = base.join(name);
        let t = read_tile(&path, &[c1.clone(), c2.clone()]).unwrap();
        assert_eq!(terrain_file(&t, &path), c1.join(format!("{name}.terrain")));
        assert_eq!(water_file(&t, &path), c1.join(format!("{name}.water")));
        let t = read_tile(&path, &[c2.clone()]).unwrap();
        assert_eq!(terrain_file(&t, &path), base.join(format!("{name}.terrain")));
        assert_eq!(water_file(&t, &path), base.join(format!("{name}.water")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn traffic_light_parents_follow_placed_signals_not_other_children() {
        let object = |file: &str, parent| omsi_map::MapObject {
            file: file.into(), var_parent: Some(parent), ..Default::default()
        };
        let tile = Tile {
            objects: vec![object("signal.sco", 10), object("sign.sco", 20)],
            attach_objects: vec![
                omsi_map::MapObject { file: "signal.sco".into(), parent_id: Some(30), ..Default::default() },
                omsi_map::MapObject { parent_id: Some(40), ..object("signal.sco", 50) },
            ],
            spline_attachments: vec![SplineAttachment {
                file: "signal.sco".into(), var_parent: Some(60), ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(traffic_light_parents(&tile, |file| file == "signal.sco"), [10, 30, 50, 60].into_iter().collect());
    }

    #[test]
    fn a_child_naming_a_light_counts_whatever_its_type() {
        // (OMSI's RefreshAmpelParenting reads the light index, not the object's kind)
        let object = |file: &str, parent, strings: &[&str]| omsi_map::MapObject {
            file: file.into(),
            var_parent: Some(parent),
            extra: strings.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let tile = Tile {
            objects: vec![
                object("mod_lamp.sco", 10, &["2"]),
                object("mod_lamp.sco", 20, &[" 0 "]),
                object("display.sco", 30, &["", ""]),
                object("sign.sco", 40, &["-1"]),
                object("sign.sco", 50, &["Hbf"]),
                object("sign.sco", 60, &[]),
            ],
            ..Default::default()
        };
        assert_eq!(light_naming_parents(&tile), [10, 20].into_iter().collect());
        assert!(traffic_light_parents(&tile, |_| false).is_empty());
        assert!(names_traffic_light(&["3".into()]) && !names_traffic_light(&["x".into()]));
    }

    #[test]
    fn a_junction_template_without_placed_signals_is_unsignalized() {
        let tile = Tile {
            objects: vec![omsi_map::MapObject { file: "junction.sco".into(), id: 10, ..Default::default() }],
            ..Default::default()
        };
        assert!(traffic_light_parents(&tile, |_| false).is_empty());
    }

    #[test]
    #[ignore = "requires OMSI_ROOT with the stock Grundorf map"]
    fn grundorf_traffic_light_parents_exclude_gaussdorf() {
        let root = PathBuf::from(std::env::var_os("OMSI_ROOT").expect("OMSI_ROOT"));
        let map_dir = root.join("maps/Grundorf");
        let global = omsi_map::GlobalCfg::load(&map_dir.join("global.cfg")).expect("Grundorf");
        let tiles = global.tiles.iter().map(|t| (t.index, t.x, t.y, map_dir.join(&t.file))).collect::<Vec<_>>();
        let index = MapIndex::build(&tiles, &[], &root);
        assert_eq!(index.tiles_failed, 0);
        assert!(index.traffic_light_parents.contains(&4174), "the real signalized junction");
        for id in [759, 761] {
            assert!(!index.traffic_light_parents.contains(&id), "Gaussdorf junction {id} has no signals");
        }
    }

    #[test]
    fn indexed_tile_search_matches_full_distance_scan() {
        let coords: Vec<(i32, i32)> = (-5..=5).flat_map(|x| (-5..=5).map(move |y| (x, y))).collect();
        let mut lookup: hashbrown::HashMap<(i32, i32), Vec<usize>> = hashbrown::HashMap::new();
        for (i, &key) in coords.iter().enumerate() {
            lookup.entry(key).or_default().push(i);
        }
        let ts = tile_size();
        for centers in [
            vec![DVec3::new(0.0, 0.0, 0.0)],
            vec![DVec3::new(-0.3 * ts, 1.8 * ts, 0.0), DVec3::new(3.2 * ts, -2.1 * ts, 0.0)],
        ] {
            for radius in [0.0, 0.7 * ts, 2.5 * ts, 20.0 * ts] {
                let expected: Vec<usize> = coords.iter().enumerate()
                    .filter_map(|(i, &(x, y))| (Streamer::nearest(&centers, x, y) <= radius).then_some(i))
                    .collect();
                let actual: Vec<usize> = tile_candidates(&lookup, coords.len(), &centers, radius)
                    .into_iter()
                    .filter(|&i| Streamer::nearest(&centers, coords[i].0, coords[i].1) <= radius)
                    .collect();
                assert_eq!(actual, expected, "centers {centers:?}, radius {radius}");
            }
        }
    }

    /// The objects row record `att` puts on its own spline `spline`.
    fn row_objects(att: &SplineAttachment, spline: &MapSpline, origin: DVec2, index: Option<&MapIndex>) -> Vec<RowObject> {
        row_start(att, spline, index).map(|start| place_on(att, spline, origin, index, start)).unwrap_or_default()
    }

    fn spline(id: i64, prev: i64, next: i64, length: f64) -> MapSpline {
        MapSpline { file: String::new(), id, prev_id: prev, next_id: next, pos: [0.0, 0.0, 10.0], heading: 0.0, length, ..Default::default() }
    }

    fn row(interval: f64, range: f64, d: f64, repeater: Option<(usize, usize)>) -> SplineAttachment {
        SplineAttachment { file: "lamp.sco".into(), id: 5, spline_index: 0, offset: [3.0, 0.25, d], interval, range, repeater, ..Default::default() }
    }

    #[test]
    fn master_row() {
        let s = spline(1, 0, 2, 100.0);
        let objs = row_objects(&row(30.0, 1000.0, 10.0, None), &s, DVec2::ZERO, None);
        let at: Vec<f64> = objs.iter().map(|o| o.pose.pos.y).collect();
        // 10, 40, 70 and 100 m: the one on the joint belongs to this spline
        assert_eq!(at.len(), 4);
        assert!((at[0] - 10.0).abs() < 1e-6 && (at[3] - 100.0).abs() < 1e-6, "{at:?}");
        // right of a spline heading north is east, the height is above the spline
        assert!((objs[0].pose.pos.x - 3.0).abs() < 1e-6 && (objs[0].pose.pos.z - 10.25).abs() < 1e-6);
        // range 45 m: objects 0 and 1 only
        assert_eq!(row_objects(&row(30.0, 45.0, 10.0, None), &s, DVec2::ZERO, None).len(), 2);
        // a single object beyond the end belongs to the next spline of the chain
        assert!(row_objects(&row(0.0, 0.0, 130.0, None), &s, DVec2::ZERO, None).is_empty());
        // ... unless the chain ends here: a little past the end is at the end
        let dead_end = spline(1, 0, 0, 100.0);
        let one = row_objects(&row(0.0, 0.0, 100.6, None), &dead_end, DVec2::ZERO, None);
        assert_eq!(one.len(), 1);
        assert!((one[0].pose.pos.y - 100.0).abs() < 1e-6);
        assert!(row_objects(&row(0.0, 0.0, 102.0, None), &dead_end, DVec2::ZERO, None).is_empty());
    }

    #[test]
    fn tangential_row_rotates_within_the_spline_plane() {
        let s = MapSpline {
            heading: 37.0, radius: 100.0, grad_start: 15.0, grad_end: -5.0,
            cant_start: 7.0, cant_end: 3.0, ..spline(1, 0, 0, 80.0)
        };
        let curve = SplineCurve::from_map(&s, DVec2::ZERO);
        let u = 25.0;
        let pitch = curve.slope_at(u).atan();
        let bank = (curve.cant_at(u) / 100.0).atan();
        let heading = curve.heading_at(u).to_radians();
        // An object's normal must not change when it is turned on that surface.
        let normal = DVec3::new(bank.sin() * pitch.cos(), -pitch.sin(), bank.cos() * pitch.cos());
        let expected = DVec3::new(
            normal.x * heading.cos() + normal.y * heading.sin(),
            -normal.x * heading.sin() + normal.y * heading.cos(), normal.z,
        ).as_vec3();
        for own_heading in [0.0, 90.0, 180.0, -90.0, 27.0] {
            let att = SplineAttachment { tilt: true, rot: [own_heading, 0.0, 0.0], ..row(0.0, 0.0, u, None) };
            let objects = row_objects(&att, &s, DVec2::ZERO, None);
            assert_eq!(objects.len(), 1);
            let rot = objects[0].pose.rot;
            assert!((rot.transform_vector3(Vec3::Z) - expected).length() < 1e-6, "heading {own_heading}");
            for axis in [Vec3::X, Vec3::Y] {
                assert!(rot.transform_vector3(axis).dot(expected).abs() < 1e-6, "heading {own_heading}");
            }
        }
    }

    #[test]
    fn tangential_row_keeps_its_frame_when_the_spline_runs_backwards() {
        let s = MapSpline { heading: 31.0, grad_start: 12.0, grad_end: 12.0,
            cant_start: 5.0, cant_end: 5.0, ..spline(1, 0, 0, 80.0) };
        let curve = SplineCurve::from_map(&s, DVec2::ZERO);
        let reverse = MapSpline { pos: curve.end_point().to_array(), heading: s.heading + 180.0,
            grad_start: -12.0, grad_end: -12.0, cant_start: -5.0, cant_end: -5.0, ..s.clone() };
        // Include the object's own pitch/bank: the spline frame must compose with
        // these too, and children inherit the resulting complete rotation.
        let att = SplineAttachment { tilt: true, rot: [90.0, 3.0, -2.0], ..row(0.0, 0.0, 20.0, None) };
        let forward = row_objects(&att, &s, DVec2::ZERO, None)[0].pose;
        let start = RowStart { s: 20.0, j: 0, backwards: true, d0: 20.0, acc: 0.0 };
        let backward = place_on(&att, &reverse, DVec2::ZERO, None, start)[0].pose;
        assert!((forward.pos - backward.pos).length() < 1e-8);
        for axis in [Vec3::X, Vec3::Y, Vec3::Z] {
            assert!((forward.rot.transform_vector3(axis) - backward.rot.transform_vector3(axis)).length() < 1e-6);
        }
    }

    #[test]
    fn upright_row_and_objects_outside_cant_width_keep_their_placement_rules() {
        let s = MapSpline { heading: 65.0, grad_start: 10.0, grad_end: 10.0,
            cant_start: 20.0, cant_end: 20.0, ..spline(1, 0, 0, 80.0) };
        let att = SplineAttachment { rot: [90.0, 3.0, -2.0], ..row(0.0, 0.0, 20.0, None) };
        let upright = row_objects(&att, &s, DVec2::ZERO, None)[0].pose;
        let expected = omsi_geometry::object_rotation([155.0, -3.0, 2.0]);
        for axis in [Vec3::X, Vec3::Y, Vec3::Z] {
            assert!((upright.rot.transform_vector3(axis) - expected.transform_vector3(axis)).length() < 1e-6);
        }
        let outside = SplineAttachment { tilt: true, offset: [11.0, 0.25, 20.0], rot: [90.0, 0.0, 0.0], ..att };
        let pose = row_objects(&outside, &s, DVec2::ZERO, None)[0].pose;
        let right = SplineCurve::dir(s.heading + 90.0).as_vec2().extend(0.0);
        assert!(pose.rot.transform_vector3(Vec3::Z).dot(right).abs() < 1e-6,
            "beyond the half cant width, the row follows only the longitudinal slope");
        assert!((pose.pos.z - 10.25).abs() < 1e-8, "the position still includes the cant's clamped height");
    }

    #[test]
    fn tangential_railing_follows_the_praha_spline() {
        // Placement of railing 26386 on spline 24096, Praha 200 tile 2623/10579.
        // Its mesh runs along local X, so the map turns it 90 degrees to the road.
        let s = MapSpline { id: 24096, pos: [163.9941, 44.16156, 270.8004],
            heading: -90.15699, length: 46.00197, grad_start: -0.5, grad_end: -0.5,
            tex_offset: 771.2027, map_chain_offset: Some(771.2027), ..Default::default() };
        let att = SplineAttachment { id: 26386, offset: [-0.0584662628010295, 0.279999999041864, 785.154450166645],
            rot: [90.0000020235813, 0.0, 0.0], tilt: true, ..Default::default() };
        let mut index = MapIndex::default();
        index.splines.insert(s.id, IndexedSpline { length: s.length, map_chain_offset: Some(s.tex_offset), prev: 0, next: 0 });
        let objects = row_objects(&att, &s, DVec2::ZERO, Some(&index));
        assert_eq!(objects.len(), 1);
        let direction = SplineCurve::dir(s.heading);
        let tangent = DVec3::new(direction.x, direction.y, -0.005).normalize().as_vec3();
        assert!((-objects[0].pose.rot.transform_vector3(Vec3::X) - tangent).length() < 1e-6,
            "the railing must descend along the kerb, not lean across it");
    }

    /// The chain of the Spandau buffer stop 3212811: a dead-end track of 250 m behind 600 m
    /// of predecessors, one of them joined end to end.
    fn buffer_stop_chain() -> MapIndex {
        let mut ix = MapIndex::default();
        ix.splines.insert(10, IndexedSpline { length: 400.0, map_chain_offset: Some(0.0), prev: 0, next: 11 });
        // spline 11 runs against the chain: its end meets 10, its start meets 12
        ix.splines.insert(11, IndexedSpline { length: 200.0, map_chain_offset: Some(250.0), prev: 12, next: 10 });
        ix.splines.insert(12, IndexedSpline { length: 250.0, map_chain_offset: Some(600.0), prev: 11, next: 0 });
        ix
    }

    #[test]
    fn row_start_counts_from_the_chain_start() {
        let mut ix = buffer_stop_chain();
        // Berlin's map links can be stale: keep the authored chain distance even when the
        // segment no longer points back to the spline that the distance includes.
        ix.splines.get_mut(&12).unwrap().prev = 0;
        assert_eq!(chain_offset(&ix, 10), 0.0);
        assert_eq!(chain_offset(&ix, 12), 600.0);
        // a row on spline 11 runs its way, from the joint with 12: the chain before it is 12
        assert_eq!(chain_offset(&ix, 11), 250.0);
        // d = 845.3 on the last spline: 4.7 m before the end of the track, not 95.3 m along
        let track = spline(12, 11, 0, 250.0);
        let objs = row_objects(&row(10.0, 0.0, 845.3, None), &track, DVec2::ZERO, Some(&ix));
        assert_eq!(objs.len(), 1);
        assert!((objs[0].pose.pos.y - 245.3).abs() < 1e-6, "{:?}", objs[0].pose.pos);
        // a loop has no start: the walk stops where it comes round
        let mut ring = MapIndex::default();
        ring.splines.insert(1, IndexedSpline { length: 10.0, map_chain_offset: None, prev: 3, next: 2 });
        ring.splines.insert(2, IndexedSpline { length: 20.0, map_chain_offset: None, prev: 1, next: 3 });
        ring.splines.insert(3, IndexedSpline { length: 30.0, map_chain_offset: None, prev: 2, next: 1 });
        assert_eq!(chain_offset(&ring, 1), 50.0);
    }

    #[test]
    fn row_runs_on_through_its_tile() {
        let mut ix = buffer_stop_chain();
        // spline 11 runs against the chain: its end meets 10 at y = 400, its start meets 12
        let s10 = spline(10, 0, 11, 400.0);
        let s11 = MapSpline { pos: [0.0, 600.0, 10.0], heading: 180.0, ..spline(11, 12, 10, 200.0) };
        let s12 = MapSpline { pos: [0.0, 600.0, 10.0], ..spline(12, 11, 0, 250.0) };
        // a row on spline 10 with objects at 20, 220, 420 and 620 m along the chain
        let master = SplineAttachment { id: 7, ..row(200.0, 600.0, 20.0, None) };
        let tile = [s10.clone(), s11.clone(), s12.clone()];
        let objs = tile_row_objects(&master, &tile, DVec2::ZERO, Some(&ix));
        let at: Vec<(usize, usize, f64)> = objs.iter().map(|(si, o)| (*si, o.index, o.pose.pos.y)).collect();
        assert_eq!(at.len(), 4, "{at:?}");
        for (k, (si, j, y)) in [(0, 0, 20.0), (0, 1, 220.0), (1, 2, 420.0), (2, 3, 620.0)].into_iter().enumerate() {
            assert!(at[k].0 == si && at[k].1 == j && (at[k].2 - y).abs() < 1e-6, "{at:?}");
        }
        // the row's right is the spline's left on the reversed spline: still east of the chain
        assert!(objs[2].1.pose.pos.x > 2.9, "{:?}", objs[2].1.pose.pos);
        // where the chain leaves the tile, the row stops (a repeater there carries it on)
        let objs = tile_row_objects(&master, &tile[..2], DVec2::ZERO, Some(&ix));
        assert_eq!(objs.iter().map(|(_, o)| o.index).collect::<Vec<_>>(), vec![0, 1, 2]);
        // ... like this one on spline 12, which then goes on through its own tile
        ix.masters.insert((3, 7), (10, 20.0));
        let repeater = SplineAttachment { id: 7, spline_index: 0, repeater: Some((3, 3)), ..row(200.0, 600.0, 20.0, None) };
        let objs = tile_row_objects(&repeater, &[s12], DVec2::ZERO, Some(&ix));
        assert_eq!(objs.len(), 1);
        assert!(objs[0].1.index == 3 && (objs[0].1.pose.pos.y - 620.0).abs() < 1e-6, "{:?}", objs[0].1.pose.pos);
        // a single object past the end of its spline goes onto the next one in the tile
        let single = SplineAttachment { id: 8, ..row(0.0, 0.0, 450.0, None) };
        let objs = tile_row_objects(&single, &tile, DVec2::ZERO, Some(&ix));
        assert_eq!(objs.len(), 1);
        assert!(objs[0].0 == 1 && (objs[0].1.pose.pos.y - 450.0).abs() < 1e-6, "{:?}", objs[0].1.pose.pos);
    }

    #[test]
    fn repeater_continues_the_row() {
        let mut ix = MapIndex::default();
        ix.splines.insert(1, IndexedSpline { length: 100.0, map_chain_offset: None, prev: 0, next: 2 });
        ix.splines.insert(2, IndexedSpline { length: 50.0, map_chain_offset: None, prev: 1, next: 3 });
        // spline 3 is joined end to end: it runs against the chain
        ix.splines.insert(3, IndexedSpline { length: 80.0, map_chain_offset: None, prev: 0, next: 2 });
        assert_eq!(chain_distance(&ix, 1, 3, 1e9), Some((150.0, true)));
        ix.masters.insert((0, 5), (1, 20.0));
        // the master row: 20, 50, 80 m on spline 1
        let s1 = spline(1, 0, 2, 100.0);
        assert_eq!(row_objects(&row(30.0, 1000.0, 20.0, None), &s1, DVec2::ZERO, None).len(), 3);
        // spline 2 (100..150 m of the chain): objects 3 (110 m) and 4 (140 m)
        let s2 = MapSpline { pos: [0.0, 100.0, 10.0], ..spline(2, 1, 3, 50.0) };
        let objs = row_objects(&row(30.0, 1000.0, 20.0, Some((0, 3))), &s2, DVec2::ZERO, Some(&ix));
        assert_eq!(objs.iter().map(|o| o.index).collect::<Vec<_>>(), vec![3, 4]);
        assert!((objs[0].pose.pos.y - 110.0).abs() < 1e-6 && (objs[1].pose.pos.y - 140.0).abs() < 1e-6);
        // on the reversed spline object 5 (170 m) lies 20 m from the joint, 60 m from its start
        let s3 = MapSpline { pos: [0.0, 230.0, 10.0], heading: 180.0, ..spline(3, 0, 2, 80.0) };
        let objs = row_objects(&row(30.0, 1000.0, 20.0, Some((0, 5))), &s3, DVec2::ZERO, Some(&ix));
        assert_eq!(objs[0].index, 5);
        assert!((objs[0].pose.pos.y - 170.0).abs() < 1e-6, "{:?}", objs[0].pose.pos);
        // the row's right is the spline's left there: still east of the chain
        assert!(objs[0].pose.pos.x > 2.9, "{:?}", objs[0].pose.pos);
    }

    #[test]
    fn stop_exit_weights_as_omsi_reads_them() {
        let v = |a: &[&str]| stop_exit_weight(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        // the stock stops: name, enter max, enter min, exit
        assert_eq!(v(&["Krankenhaus", "0", "0", "10", "", "", ""]), 10.0);
        // no exit number: the mean of the two entering numbers
        assert_eq!(v(&["A", "6", "2"]), 4.0);
        assert_eq!(v(&["A", "", ""]), 0.5);
        // rounded, never below 0, rubbish ignored
        assert_eq!(v(&["A", "1", "0", "2.6"]), 3.0);
        assert_eq!(v(&["A", "1", "0", "-4"]), 0.0);
        assert_eq!(v(&["A", "1", "0", "x"]), 0.5);
    }

    #[test]
    fn stop_side_as_omsi_reads_it() {
        let v = |a: &[&str]| stop_side(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        // Urumqi61's shape: name, enter max, enter min, exit, ?, side, "", ""
        assert_eq!(v(&["NianZiGou", "10", "0", "", "80", "1", "", ""]), 1.0);
        assert_eq!(v(&["RenMinGuangChang", "50", "20", "100", "80", "0", "", ""]), 0.0);
        // both sides
        assert_eq!(v(&["A", "10", "0", "", "30", "2", "", ""]), 2.0);
        // nothing said / rubbish / a short block: the right-hand side, as OMSI's default
        assert_eq!(v(&["A", "10", "0", "", "30"]), 0.0);
        assert_eq!(v(&["A", "10", "0", "", "30", "", "", ""]), 0.0);
        assert_eq!(v(&["bss1\\11.jpg", "bss1\\6.jpg", "", "", "", "", "", ""]), 0.0);
        assert_eq!(v(&["A", "10", "0", "", "30", "x", "", ""]), 0.0);
        // a value out of range is clamped, not trusted into a side that does not exist
        assert_eq!(v(&["A", "10", "0", "", "30", "80", "", ""]), 2.0);
    }

    #[test]
    fn attachment_order() {
        let a = omsi_scenery::sco::Attachment { ops: vec![("attach_rot_z".into(), vec![180.0]), ("attach_trans".into(), vec![-4.0, 0.4, 5.0])] };
        let m = attachment_matrix(&a);
        let p = m.transform_point3(Vec3::ZERO);
        assert!((p - Vec3::new(-4.0, 0.4, 5.0)).length() < 1e-5, "{p:?}");
        // the attached object is turned round, its forward axis points backwards
        assert!((m.transform_vector3(Vec3::Y) - Vec3::new(0.0, -1.0, 0.0)).length() < 1e-5);
        let parent = Pose { pos: DVec3::new(100.0, 200.0, 30.0), rot: omsi_geometry::object_rotation([90.0, 0.0, 0.0]) };
        let child = parent.attached(&m, [0.0; 3]);
        // parent faces east: its left (-x) is north
        assert!((child.pos - DVec3::new(100.4, 204.0, 35.0)).length() < 1e-4, "{:?}", child.pos);
        assert!((child.heading() - 270.0).abs() < 1e-3, "{}", child.heading());
    }
}
