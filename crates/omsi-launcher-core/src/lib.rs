//! The launcher's data side: what the window asks for (maps, buses, lines, tours, the
//! roadbook, weather, profiles, settings) and how a duty is turned into a command line for
//! the game. Everything here is plain functions over the OMSI content crates; the window
//! (the game binary's `launcher` module, drawn with wgpu) calls them directly, and
//! `cli()` exposes the same functions to a terminal (`omsi-launcher --cli ...`).
//!
//! `install` runs mod installs as background jobs, `index` caches the content lists and
//! tells the page when they changed, `instances` keeps track of the games started.

pub mod index;
pub mod install;
pub mod instances;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------------------
// configuration: where the game and the OMSI content are

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    /// The OMSI 2 folder (maps, Vehicles, ...).
    pub root: String,
    /// The `omsi` game binary.
    pub game: String,
    /// The current profile name.
    pub profile: String,
}

fn home() -> PathBuf {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_default()
}

pub fn data_dir() -> PathBuf {
    let d = home().join(".openomsi");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn config_path() -> PathBuf {
    data_dir().join("launcher.json")
}

/// A complete installation of the original OMSI 2 (the same test the game makes: it refuses
/// to start on anything else).
fn is_omsi_root(p: &Path) -> bool {
    omsi_cfg::missing_original_essentials(p).is_empty()
}

/// Where the game binary is: the configured path, next to the launcher, or the
/// development build in the source tree.
fn find_game(configured: &str) -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    // the game that came with this launcher first: a path remembered from an older
    // installation (`target/release/omsi` of the days before the rename) kept starting an
    // old build after every update - the new pause menu "was not there" on macOS
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join(if cfg!(windows) { "openomsi.exe" } else { "openomsi" }));
        }
    }
    if !configured.trim().is_empty() {
        let c = PathBuf::from(configured.trim());
        // (only a game of today's name: the old `omsi` binary is not taken any more)
        let stem = c.file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        if stem == "openomsi" {
            cands.push(c);
        }
    }
    if let Some(p) = std::env::var_os("OPENOMSI_BIN") {
        cands.push(PathBuf::from(p));
    }
    if let Ok(exe) = std::env::current_exe() {
        for a in exe.ancestors().skip(1).take(7) {
            cands.push(a.join("openomsi"));
            cands.push(a.join("openomsi.exe"));
            cands.push(a.join("target").join("release").join("openomsi"));
            cands.push(a.join("target").join("release").join("openomsi.exe"));
            cands.push(a.join("Resources").join("openomsi"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        for a in cwd.ancestors().take(4) {
            cands.push(a.join("target").join("release").join("openomsi"));
        }
    }
    cands.push(data_dir().join("openomsi"));
    cands.into_iter().find(|p| p.is_file())
}

/// The last search for the OMSI folder: (the places given first, what was found).
static FOUND: std::sync::Mutex<Option<(Vec<PathBuf>, Option<PathBuf>)>> = std::sync::Mutex::new(None);

/// The OMSI folder: configured, remembered by the game, or found in any usual place
/// (beside the program, Steam libraries, Wine bottles, the user's folders).
fn find_root(configured: &str) -> Option<PathBuf> {
    let mut first: Vec<PathBuf> = Vec::new();
    if !configured.trim().is_empty() {
        first.push(PathBuf::from(configured.trim()));
    }
    if let Some(p) = std::env::var_os("OMSI_ROOT") {
        first.push(PathBuf::from(p));
    }
    if let Ok(t) = std::fs::read_to_string(home().join(".openomsi-root")) {
        first.push(PathBuf::from(t.trim()));
    }
    // searching the disk costs a moment: once per process is enough (until the settings
    // are saved again, see `save_config`)
    let mut g = FOUND.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((k, v)) = g.as_ref() {
        if *k == first && v.as_ref().map(|p| is_omsi_root(p)).unwrap_or(true) {
            return v.clone();
        }
    }
    let r = omsi_cfg::find_original_install(&first);
    if let Some(p) = &r {
        // the game finds it the same way next time
        let _ = std::fs::write(home().join(".openomsi-root"), p.to_string_lossy().as_bytes());
    }
    *g = Some((first, r.clone()));
    r
}

pub fn load_config() -> Config {
    let mut c: Config = std::fs::read_to_string(config_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    if let Some(r) = find_root(&c.root) {
        c.root = r.to_string_lossy().to_string();
    }
    if let Some(g) = find_game(&c.game) {
        c.game = g.to_string_lossy().to_string();
    }
    if c.profile.trim().is_empty() {
        // a first start takes the driver OMSI 2 had last ([last_driver] of its options.cfg)
        c.profile = Path::new(&c.root)
            .is_dir()
            .then(|| omsi_options(Path::new(&c.root)))
            .flatten()
            .and_then(|o| o.last_driver)
            .unwrap_or_else(|| "Driver".into());
    }
    c
}

/// What the player's own OMSI 2 remembers in its `options.cfg`: the settings in the
/// launcher's keys, the map and the driver played last.
pub struct OmsiOptions {
    pub settings: Value,
    /// `maps/<name>/global.cfg`, as the launcher names maps.
    pub last_map: Option<String>,
    /// The personnel file's name without `.odr`.
    pub last_driver: Option<String>,
}

/// Read the original's `options.cfg` (never written: the game keeps its own settings).
pub fn omsi_options(root: &Path) -> Option<OmsiOptions> {
    let o = omsi_content::options::Options::load(&root.join("options.cfg")).ok()?;
    let mut v = json!({});
    let num = |k: &str| o.str(k).and_then(|x| x.trim().replace(',', ".").parse::<f64>().ok()).filter(|x| x.is_finite());
    // (not on a phone: the PC's OMSI caps at 30, and a phone played at 30 frames)
    if let Some(x) = num("maxfps").filter(|_| !cfg!(target_os = "android")) {
        v["max_fps"] = json!(x.max(0.0) as i64);
    }
    if let Some(x) = num("performance_minobjsize") {
        v["min_obj_size"] = json!(x.clamp(0.0, 0.2));
    }
    if let Some(x) = num("performance_maxobjdist") {
        v["max_obj_dist"] = json!((x.round() as i64).max(0).to_string());
    }
    if let Some(af) = o.values.get("texfilter").and_then(|x| x.get(1)).and_then(|x| x.trim().parse::<i64>().ok()) {
        v["anisotropy"] = json!(af.clamp(1, 16));
    }
    if let Some(x) = num("texmemlimit").filter(|x| *x > 0.0) {
        v["texture_memory"] = json!(x as i64);
    }
    if let Some(x) = num("performance_refltexsize") {
        v["mirror_size"] = json!(1i64 << (x as i64).clamp(6, 11));
    }
    if let Some(x) = o.str("performance_realreflexions") {
        v["mirror_refresh"] = json!(mirror_refresh(x));
    }
    if let Some(x) = num("sound_vol_master") {
        v["volume"] = json!(x.clamp(0.0, 1.0));
    }
    if let Some(d) = o.str("sound_doppler") {
        v["doppler"] = json!(!d.trim().eq_ignore_ascii_case("off"));
    }
    if let Some(l) = o.str("language").filter(|l| !l.trim().is_empty()) {
        v["language"] = json!(language_code(l));
    }
    if let Some(x) = num("wear_lifespan") {
        v["maintenance"] = json!((x as i64).clamp(0, 4));
    }
    if let Some(x) = num("aiunschedfactor") {
        v["ai_unsched_factor"] = json!((x as i64).clamp(0, 300));
    }
    if let Some(x) = num("aimaxcountscheduled") {
        v["ai_max_scheduled"] = json!((x as i64).max(0));
    }
    // (the second line of [AIMaxCountRandom]: the people Omsi.exe makes)
    if let Some(x) = o.values.get("aimaxcountrandom").and_then(|x| x.get(1)).and_then(|x| x.trim().parse::<i64>().ok()) {
        v["ai_max_humans"] = json!(x.max(1));
    }
    if let Some(x) = num("aimaxcountparked") {
        v["ai_max_parked"] = json!((x as i64).max(0));
    }
    if let Some(x) = num("aipassfactor") {
        v["pax_density"] = json!((x / 100.0).clamp(0.0, 3.0));
    }
    // flags: present or not
    v["head_movement"] = json!(o.flag("driverview_moving"));
    v["driverview_smooth"] = json!(o.flag("driverview_smooth"));
    v["collision_vehicles"] = json!(!o.flag("no_collision_vehtoveh"));
    v["collision_objects"] = json!(!o.flag("no_collision"));
    v["driver"] = json!(o.flag("see_own_driver"));
    if let Some(x) = num("ticketselling") {
        v["boarding"] = json!(if x > 0.5 { "pay" } else { "auto" });
    }
    let file_stem = |p: &str| Path::new(&p.replace('\\', "/")).file_stem().map(|s| s.to_string_lossy().to_string());
    Some(OmsiOptions {
        settings: v,
        last_map: o.str("last_map").map(|m| m.trim().replace('\\', "/")).filter(|m| !m.is_empty()),
        last_driver: o.str("last_driver").and_then(file_stem).filter(|d| !d.is_empty()),
    })
}

pub fn save_config(c: &Config) -> Result<()> {
    std::fs::write(config_path(), serde_json::to_vec_pretty(c)?)?;
    // the folder is looked for again: a folder that was not a complete OMSI 2 when it was
    // first chosen (still being copied, a part missing) and is now stayed "not found" until
    // the launcher was restarted, however often it was chosen and saved again
    *FOUND.lock().unwrap_or_else(|e| e.into_inner()) = None;
    Ok(())
}

fn root() -> Result<PathBuf> {
    let c = load_config();
    let r = PathBuf::from(&c.root);
    if c.root.trim().is_empty() {
        return Err(anyhow!("no OMSI 2 folder configured (set it under Setup)"));
    }
    let missing = omsi_cfg::missing_original_essentials(&r);
    if !missing.is_empty() {
        return Err(anyhow!(
            "{} is not a complete OMSI 2 installation (missing: {}); choose the original game's folder under Setup",
            r.display(),
            missing.join(", ")
        ));
    }
    Ok(r)
}

/// The game's own content folder: the folder of the game binary, laid out like OMSI 2
/// (Vehicles, maps, Sceneryobjects ...). Installed mods live here; the game searches it
/// before the original installation.
pub fn content_dir() -> Option<PathBuf> {
    // the same rules as the game: $OMSI_CONTENT, else the folder of the game binary (beside
    // the bundle when the binary sits inside a macOS .app)
    let dir = match std::env::var_os("OMSI_CONTENT") {
        Some(d) => PathBuf::from(d),
        None => {
            let c = load_config_raw();
            let game = find_game(&c.game)?;
            let dir = game.parent()?.to_path_buf();
            let beside = if dir.ends_with("Contents/MacOS") { dir.parent()?.parent()?.parent()?.to_path_buf() } else { dir };
            let cand = omsi_cfg::content_folder_of(&beside);
            if !omsi_cfg::is_programs_folder(&cand) && (cand.exists() || std::fs::create_dir_all(&cand).is_ok()) && omsi_cfg::is_writable(&cand) {
                cand
            } else {
                data_dir().join("content")
            }
        }
    };
    let _ = omsi_cfg::ensure_content_layout(&dir);
    register_roots(&dir);
    Some(dir)
}

/// Tell the OMSI readers about the content folder, the archives used in place in its
/// `Archives` folder and the OMSI folder (in that order, as the game has them), so that a
/// path inside one is also looked for in the others (a repaint's `.cti` in the content
/// folder's copy of a stock bus folder, a map inside an archive).
fn register_roots(content: &Path) {
    static DONE: std::sync::Once = std::sync::Once::new();
    let content = content.to_path_buf();
    DONE.call_once(|| {
        omsi_cfg::add_content_root(content.clone());
        mount_archives(&content);
    });
    // the OMSI folder - also one chosen under Setup while the launcher runs (it was only
    // looked for once, at the start, and a folder set later was never a root)
    if let Some(r) = find_root(&load_config_raw().root) {
        omsi_cfg::add_content_root(r);
    }
}

/// Mount the archives in `<content>/Archives` that are not mounted yet (an install may
/// have put one there, or the user did), each as a content root in front of the OMSI
/// folder. Archives that went away stay mounted until the launcher restarts.
fn mount_archives(content: &Path) {
    let dir = content.join(install::ARCHIVES);
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    let mut zips: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_file() && p.extension().map(|e| e.eq_ignore_ascii_case("zip")).unwrap_or(false)).collect();
    zips.sort();
    let mounted: Vec<PathBuf> = omsi_cfg::vfs::mounts().iter().map(|m| m.path().to_path_buf()).collect();
    for z in zips.into_iter().filter(|z| !mounted.contains(z)) {
        mount_archive(&z);
    }
}

/// Mount one archive of the content folder for the lists.
pub(crate) fn mount_archive(zip: &Path) {
    match omsi_cfg::vfs::mount_zip(zip) {
        Ok(m) => match find_root(&load_config_raw().root) {
            Some(r) => omsi_cfg::add_content_root_before(m, &r),
            None => omsi_cfg::add_content_root(m),
        },
        Err(e) => {
            log_to_file(&format!("archive {}: {e}", zip.display()));
        }
    }
}

/// The archives of the content folder that are mounted, in name order.
fn archive_roots(content: &Path) -> Vec<PathBuf> {
    let dir = content.join(install::ARCHIVES);
    let mut v: Vec<PathBuf> = omsi_cfg::vfs::mounts().iter().map(|m| m.path().to_path_buf()).filter(|p| p.starts_with(&dir) && p.exists()).collect();
    v.sort();
    v
}

fn load_config_raw() -> Config {
    std::fs::read_to_string(config_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// `rel` (a path as the game takes it, `Vehicles/Foo/foo.bus`) in the content folder if it
/// is there, else in the OMSI folder.
fn resolve_content(rel: &str) -> Result<PathBuf> {
    let root = root()?;
    for b in bases() {
        if b == root {
            continue;
        }
        let p = omsi_cfg::resolve_path(&b, rel);
        if omsi_cfg::vfs::exists(&p) {
            return Ok(p);
        }
    }
    Ok(omsi_cfg::resolve_path(&root, rel))
}

/// The folders the lists are made of: the content folder first, then the archives used in
/// place (mounted as folders), then the OMSI folder.
fn bases() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(c) = content_dir() {
        mount_archives(&c);
        v.push(c.clone());
        v.extend(archive_roots(&c));
    }
    if let Ok(r) = root() {
        v.push(r);
    }
    v
}

/// Entries of `rel` (e.g. "Vehicles") across the content folder and the OMSI 2 folder;
/// a name in the content folder hides the same name in the installation.
fn merged_entries(rel: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let dirs: Vec<PathBuf> = bases().iter().map(|b| b.join(rel)).collect();
    for d in dirs {
        for (name, _) in omsi_cfg::vfs::list_dir(&d).unwrap_or_default() {
            if seen.insert(name.to_string_lossy().to_ascii_lowercase()) {
                out.push(d.join(name));
            }
        }
    }
    out.sort();
    out
}

/// Folders of `rel` by name across the content folder and the OMSI folder: each name with
/// all its copies, the content folder's first (a mod may add files to a stock folder).
fn merged_folders(rel: &str) -> Vec<(String, Vec<PathBuf>)> {
    let mut out: Vec<(String, Vec<PathBuf>)> = Vec::new();
    let mut at: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for base in bases() {
        let dir = base.join(rel);
        let Some(list) = omsi_cfg::vfs::list_dir(&dir) else { continue };
        let mut names: Vec<PathBuf> = list.into_iter().filter(|(_, is_dir)| *is_dir).map(|(n, _)| dir.join(n)).collect();
        names.sort();
        for p in names {
            let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            // (by an index: thousands of folders compared each with all before took long)
            match at.get(&name.to_ascii_lowercase()) {
                Some(&i) => out[i].1.push(p),
                None => {
                    at.insert(name.to_ascii_lowercase(), out.len());
                    out.push((name, vec![p]));
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.to_ascii_lowercase().cmp(&b.0.to_ascii_lowercase()));
    out
}

/// Is `p` in the content folder (a mod) rather than the OMSI folder?
/// (An archive used in place lies in the content folder's `Archives` too.)
fn in_content(p: &Path) -> bool {
    content_dir().map(|c| p.starts_with(c)).unwrap_or(false)
}

// ---------------------------------------------------------------------------------------
// mods: sorting a mod's folders into the content folder, the way it would be copied into
// an OMSI 2 installation by hand

#[derive(Serialize, Clone, Debug)]
pub struct ModsStatus {
    pub content_dir: String,
    /// (folder, number of entries)
    pub folders: Vec<(String, usize)>,
    pub inbox: String,
    /// What lies in the inbox now.
    pub inbox_items: Vec<String>,
    /// Packs waiting in Mods/waiting for the bus they belong to.
    pub waiting: Vec<String>,
    /// Archives used in place (in `<content>/Archives`): (name, size in bytes).
    pub archives: Vec<(String, u64)>,
    /// Free space on the content folder's disk (bytes).
    pub free_bytes: u64,
    /// What an earlier, interrupted install left and was removed now.
    pub cleaned: Vec<String>,
    pub jobs: Vec<install::Progress>,
}

/// Start installing the mod at `src` (a folder, .zip, .7z or .rar) into the content folder in the
/// background; `mode` is "auto" (unpack, or use a .zip in place when it does not fit; .7z
/// and .rar are always unpacked),
/// "extract" or "inplace".
pub fn start_install(src: &Path, mode: &str) -> Result<install::Progress> {
    let content = content_dir().ok_or_else(|| anyhow!("no game binary configured, so no content folder"))?;
    if !src.exists() {
        return Err(anyhow!("{} does not exist", src.display()));
    }
    let from_inbox = src.starts_with(content.join("Mods"));
    let job = install::start(content, root().ok(), src.to_path_buf(), install::InstallMode::parse(mode), from_inbox);
    Ok(job.snapshot())
}

/// How big the mod at `src` is unpacked, what is free, and whether it can be used in place.
pub fn inspect_mod(src: &Path) -> Result<install::SourceInfo> {
    let content = content_dir().ok_or_else(|| anyhow!("no game binary configured, so no content folder"))?;
    install::inspect(&content, root().ok().as_deref(), src)
}

/// Install the mod at `src` and wait for it (the terminal), printing the progress.
pub fn install_mod_blocking(src: &Path, mode: &str, cancel_after_ms: Option<u64>) -> Result<install::Progress> {
    let content = content_dir().ok_or_else(|| anyhow!("no game binary configured, so no content folder"))?;
    Ok(install::run_blocking(content, root().ok(), src.to_path_buf(), install::InstallMode::parse(mode), cancel_after_ms.map(std::time::Duration::from_millis), true))
}

fn inbox_entries(content: &Path) -> Vec<PathBuf> {
    let inbox = content.join("Mods");
    let Ok(rd) = std::fs::read_dir(&inbox) else { return Vec::new() };
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
            !(name.starts_with('.') || name.eq_ignore_ascii_case("installed") || name.eq_ignore_ascii_case(install::WAITING) || name.eq_ignore_ascii_case(install::PLUGINS_HELD) || name.eq_ignore_ascii_case(install::UNINSTALLED) || name.eq_ignore_ascii_case("README.txt"))
                && (p.is_dir() || p.extension().map(|x| ["zip", "7z", "rar"].iter().any(|ext| x.eq_ignore_ascii_case(ext))).unwrap_or(false))
        })
        .collect();
    v.sort();
    v
}

/// (size, newest time, files) of a file or a folder tree.
fn tree_signature(p: &Path) -> (u64, u64, u64) {
    let Ok(md) = std::fs::symlink_metadata(p) else { return (0, 0, 0) };
    let t = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos() as u64).unwrap_or(0);
    if !md.is_dir() {
        return (md.len(), t, 1);
    }
    let mut acc = (0, t, 0);
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let (s, m, n) = tree_signature(&e.path());
            acc = (acc.0 + s, acc.1.max(m), acc.2 + n);
        }
    }
    acc
}

/// The inbox watcher: something dropped into Mods/ is installed once it has stopped
/// growing (the same size and time on two looks at least two seconds apart); packs in
/// Mods/waiting are installed once their bus is. Returns the sources of the jobs started.
fn watch_inbox(content: &Path) -> Vec<String> {
    struct Seen {
        sig: (u64, u64, u64),
        at: std::time::Instant,
        started: bool,
        /// When the signature was last read (a big folder whose install failed is looked
        /// at again only now and then).
        checked: std::time::Instant,
    }
    static SEEN: std::sync::Mutex<Option<std::collections::HashMap<PathBuf, Seen>>> = std::sync::Mutex::new(None);
    // a mod deleted from Mods/installed is taken out of the lists (#819)
    let gone = install::uninstall_removed(content);
    for g in &gone {
        log_line(&format!("mods: {g} was deleted from Mods/installed - uninstalled, its folders are in Mods/{}/{g}", install::UNINSTALLED));
    }
    if !gone.is_empty() {
        omsi_cfg::content_changed();
    }
    let mut started = Vec::new();
    let mut guard = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let seen = guard.get_or_insert_with(Default::default);
    let waiting_dir = content.join("Mods").join(install::WAITING);
    let items = inbox_entries(content);
    seen.retain(|p, _| items.contains(p) || (p.starts_with(&waiting_dir) && p.exists()));
    for p in items {
        if install::is_busy(&p) {
            continue;
        }
        let now = std::time::Instant::now();
        if seen.get(&p).map(|s| s.started && now.duration_since(s.checked) < std::time::Duration::from_secs(30)).unwrap_or(false) {
            continue;
        }
        let sig = tree_signature(&p);
        if let Some(s) = seen.get_mut(&p) {
            s.checked = now;
        }
        match seen.get_mut(&p) {
            Some(s) if s.sig == sig => {
                if !s.started && now.duration_since(s.at) >= std::time::Duration::from_secs(2) {
                    s.started = true;
                    install::start(content.to_path_buf(), root().ok(), p.clone(), install::InstallMode::Auto, true);
                    started.push(p.to_string_lossy().to_string());
                }
            }
            // new, still growing (a copy in progress), or changed after a failed try
            _ => {
                seen.insert(p.clone(), Seen { sig, at: now, started: false, checked: now });
            }
        }
    }
    let busy = install::jobs().iter().any(|j| j.finished.is_none());
    if !busy {
        for p in install::waiting_ready(content, root().ok().as_deref()) {
            let sig = tree_signature(&p);
            if seen.get(&p).map(|s| s.started && s.sig == sig).unwrap_or(false) {
                continue;
            }
            seen.insert(p.clone(), Seen { sig, at: std::time::Instant::now(), started: true, checked: std::time::Instant::now() });
            install::start(content.to_path_buf(), root().ok(), p.clone(), install::InstallMode::Extract, true);
            started.push(p.to_string_lossy().to_string());
        }
    }
    started
}

/// Install everything in the inbox (and the waiting packs whose bus is there) now, and
/// wait (the terminal's `mods`).
pub fn install_inbox_blocking() -> Vec<install::Progress> {
    let Some(content) = content_dir() else { return Vec::new() };
    let mut out = Vec::new();
    for p in inbox_entries(&content).into_iter().chain(install::waiting_ready(&content, root().ok().as_deref())) {
        let job = install::start(content.clone(), root().ok(), p, install::InstallMode::Auto, true);
        while job.snapshot().finished.is_none() {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        out.push(job.snapshot());
    }
    out
}

pub fn mods_status() -> Result<ModsStatus> {
    let content = content_dir().ok_or_else(|| anyhow!("no game binary configured, so no content folder"))?;
    let cleaned = install::cleanup_stale(&data_dir(), Some(&content));
    let folders = omsi_cfg::CONTENT_FOLDERS
        .iter()
        .map(|f| {
            let n = std::fs::read_dir(content.join(f)).map(|rd| rd.flatten().filter(|e| !e.file_name().to_string_lossy().starts_with('.')).count()).unwrap_or(0);
            (f.to_string(), n)
        })
        .collect();
    let names = |v: Vec<PathBuf>| v.iter().filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string())).collect::<Vec<_>>();
    let waiting: Vec<PathBuf> = std::fs::read_dir(content.join("Mods").join(install::WAITING)).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    let mut archives: Vec<(String, u64)> = std::fs::read_dir(content.join(install::ARCHIVES))
        .map(|rd| rd.flatten().filter(|e| e.path().extension().map(|x| x.eq_ignore_ascii_case("zip")).unwrap_or(false)).map(|e| (e.file_name().to_string_lossy().to_string(), e.metadata().map(|m| m.len()).unwrap_or(0))).collect())
        .unwrap_or_default();
    archives.sort();
    Ok(ModsStatus {
        content_dir: content.to_string_lossy().to_string(),
        folders,
        inbox: content.join("Mods").to_string_lossy().to_string(),
        inbox_items: names(inbox_entries(&content)),
        waiting: names(waiting),
        archives,
        free_bytes: install::free_space(&content).unwrap_or(0),
        cleaned,
        jobs: install::jobs(),
    })
}

/// What the page asks every few seconds: whether the content changed (then it asks for the
/// lists again), the install jobs and the running games. It also starts the inbox installs.
#[derive(Serialize, Clone, Debug)]
pub struct Poll {
    pub stamp: String,
    pub jobs: Vec<install::Progress>,
    pub instances: Vec<instances::Instance>,
    /// Inbox items whose install started with this poll.
    pub started: Vec<String>,
}

pub fn poll() -> Result<Poll> {
    let content = content_dir();
    let started = content.as_deref().map(watch_inbox).unwrap_or_default();
    let stamp = index::content_stamp(&bases(), content.as_ref().map(|c| c.join("Mods")).as_deref());
    Ok(Poll { stamp, jobs: install::jobs(), instances: instances::list(), started })
}

// ---------------------------------------------------------------------------------------
// content lists

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MapInfo {
    pub name: String,
    pub friendly: String,
    pub file: String,
    pub description: String,
    pub entry_points: Vec<EntryInfo>,
    /// The depot file (`.hof` name) the map's own buses use, from ailists.cfg.
    pub hof: String,
    /// Installed as a mod (in the content folder).
    #[serde(default)]
    pub installed: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct EntryInfo {
    pub index: i32,
    pub name: String,
}

pub fn list_maps() -> Result<Vec<MapInfo>> {
    root()?;
    let lang = content_language();
    let mut out = Vec::new();
    let mut keys = Vec::new();
    for (folder, dirs) in merged_folders("maps") {
        // a map is one folder: the first copy that has a global.cfg
        let Some(d) = dirs.into_iter().find(|d| omsi_cfg::vfs::is_file(&d.join("global.cfg"))) else { continue };
        let key = format!("map|{lang}|{}", d.display());
        let mut stamped = vec![d.clone(), d.join("global.cfg"), d.join("ailists.cfg")];
        stamped.extend(dsc_candidates(&d.join("global.cfg"), lang));
        let stamp = index::files_stamp(&stamped);
        keys.push(key.clone());
        let info: Option<MapInfo> = index::cached(&key, stamp, || (read_map(&d, &folder, lang), Vec::new()));
        out.extend(info);
    }
    index::save("map|", Some(&keys));
    if out.is_empty() {
        log_empty("maps", "global.cfg");
    }
    Ok(out)
}

/// Say in launcher.log where a list that came out empty was looked for: each folder of
/// `rel` and how many entries it has (a player whose lists stay empty sends the log).
fn log_empty(rel: &str, what: &str) {
    let places: Vec<String> = bases()
        .iter()
        .map(|b| {
            let d = b.join(rel);
            match omsi_cfg::vfs::list_dir(&d) {
                Some(l) => format!("{} ({} entries)", d.display(), l.len()),
                None => format!("{} (cannot be read: {})", d.display(), std::fs::read_dir(&d).err().map(|e| e.to_string()).unwrap_or_else(|| "not a folder".into())),
            }
        })
        .collect();
    log_line(&format!("{rel}: nothing with a {what} found in {}", if places.is_empty() { "no folder (no OMSI 2 folder and no content folder)".to_string() } else { places.join(", ") }));
}

fn read_map(d: &Path, folder: &str, lang: &str) -> Option<MapInfo> {
    let g = match omsi_map::GlobalCfg::load(&d.join("global.cfg")) {
        Ok(g) => g,
        Err(e) => {
            log_line(&format!("maps: {} cannot be read ({e:#}) - not listed", d.join("global.cfg").display()));
            return None;
        }
    };
    // `global_ENG.dsc` (the language of the settings) names and describes the map
    let dsc = find_dsc(&d.join("global.cfg"), lang);
    let friendly = dsc.as_ref().and_then(|x| x.name.first().cloned()).unwrap_or_else(|| g.friendly_name.trim().to_string());
    let description = dsc.map(|x| x.description).filter(|t| !t.is_empty()).unwrap_or_else(|| g.description.trim().to_string());
    let mut entries: Vec<EntryInfo> = g.entry_points.iter().map(|e| EntryInfo { index: e.index, name: e.name.trim().to_string() }).collect();
    // the game's --entry is the position in the list, not the index field; several
    // entries often share a name (one per stop position), so number them
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let total: std::collections::HashMap<String, usize> = entries.iter().fold(std::collections::HashMap::new(), |mut m, e| {
        *m.entry(e.name.clone()).or_default() += 1;
        m
    });
    for (i, e) in entries.iter_mut().enumerate() {
        e.index = i as i32;
        let n = seen.entry(e.name.clone()).or_default();
        *n += 1;
        if total.get(&e.name).copied().unwrap_or(0) > 1 {
            e.name = format!("{} ({})", e.name, n);
        }
    }
    // (the depot groups' first: a plain car group's name line is no depot)
    let hof = omsi_map::ailists::AiLists::load(&d.join("ailists.cfg")).ok().and_then(|l| l.groups.iter().filter(|g| g.is_depot).chain(l.groups.iter()).find_map(|g| g.hof.clone())).unwrap_or_default();
    Some(MapInfo { name: if g.name.trim().is_empty() { folder.to_string() } else { g.name.trim().to_string() }, friendly, file: format!("maps/{folder}/global.cfg"), description, entry_points: entries, hof, installed: in_content(d) })
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct VehicleInfo {
    pub name: String,
    pub manufacturer: String,
    pub type_name: String,
    pub file: String,
    pub folder: String,
    pub description: String,
    /// The original third [friendlyname] line, shown for the bus's standard paint.
    #[serde(default)]
    pub default_paint: String,
    pub paints: Vec<String>,
    pub hofs: Vec<String>,
    /// Installed as a mod (the bus file is in the content folder).
    #[serde(default)]
    pub installed: bool,
    /// Vehicle packs this bus borrows parts from that are not installed (the Ahlheim
    /// Citaro's dashboard, steering wheel and ticket machine come from `Urbino_II`): it
    /// drives, but with holes in the cockpit, as it would in OMSI.
    #[serde(default)]
    pub missing_packs: Vec<String>,
    /// The fleet numbers of the bus's `[number]` list with the plate each comes with
    /// (Omsi.exe's number combo in the vehicle dialog; empty: the bus has no list).
    #[serde(default)]
    pub numbers: Vec<(String, String)>,
}

/// Names in older vehicle packs use underscores as spaces.
pub fn display_bus_name(name: &str) -> String {
    name.replace('_', " ").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The displayed vehicle type, using its file name when [friendlyname] leaves it empty.
pub fn vehicle_type_label(type_name: &str, path: &Path) -> String {
    if type_name.trim().is_empty() {
        display_bus_name(&path.file_stem().unwrap_or_default().to_string_lossy())
    } else {
        display_bus_name(type_name)
    }
}

/// The vehicle packs whose parts a model file names and that are installed nowhere.
fn missing_packs_of(model: &Path) -> Vec<String> {
    let Ok(text) = omsi_cfg::vfs::read(model) else { return Vec::new() };
    let text = omsi_cfg::codepage::decode(&text);
    let dir = model.parent().unwrap_or(Path::new("."));
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        if !l.starts_with("..") {
            continue;
        }
        let p = omsi_cfg::resolve_path(dir, l);
        // (the game also finds a part from the model's parent folders - `<vehicle>/model`
        // and the vehicle folder for a cfg in `model/Configuration Files`: mesh_path)
        let found = |d: &Path| omsi_cfg::vfs::is_file(&omsi_cfg::resolve_path(d, l));
        if omsi_cfg::vfs::is_file(&p) || dir.ancestors().skip(1).take(2).any(found) {
            continue;
        }
        if let Some(pack) = omsi_cfg::missing_vehicle_pack(&p) {
            if !out.contains(&pack) {
                out.push(pack);
            }
        }
    }
    out
}

/// `[item]` names of every `.cti` in the model's `[CTC]` folders (in every content root):
/// the paint schemes, and the folders they were read from.
fn paint_schemes(vehicle: &omsi_vehicle::Vehicle) -> (Vec<String>, Vec<PathBuf>) {
    let mut names: Vec<String> = Vec::new();
    let mut dirs_read: Vec<PathBuf> = Vec::new();
    let Some(model_rel) = vehicle.model.as_ref() else { return (names, dirs_read) };
    let model_path = omsi_cfg::resolve_path(vehicle.dir(), model_rel);
    let Ok(model) = omsi_model::Model::load(&model_path) else { return (names, dirs_read) };
    for c in &model.ctc {
        let dir = omsi_cfg::resolve_path(vehicle.dir(), &c.path);
        let mut files: Vec<PathBuf> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut copies = omsi_cfg::mirrored_dirs(&dir);
        if !copies.contains(&dir) {
            copies.push(dir.clone());
        }
        for d in copies {
            dirs_read.push(d.clone());
            let Some(list) = omsi_cfg::vfs::list_dir(&d) else { continue };
            let mut here: Vec<PathBuf> = list.into_iter().map(|(n, _)| d.join(n)).filter(|p| p.extension().map(|e| e.eq_ignore_ascii_case("cti")).unwrap_or(false)).collect();
            here.sort();
            for f in here {
                if seen.insert(f.file_name().unwrap_or_default().to_string_lossy().to_ascii_lowercase()) {
                    files.push(f);
                }
            }
        }
        for f in files {
            let Ok(cfg) = omsi_cfg::CfgFile::read(&f) else { continue };
            let mut r = cfg.reader();
            while let Some(k) = r.next_keyword() {
                if k == "item" {
                    let name = r.str().trim().to_string();
                    if !name.is_empty() && !names.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
                        names.push(name);
                    }
                }
            }
        }
    }
    dirs_read.sort();
    dirs_read.dedup();
    (names, dirs_read)
}

/// One line into ~/.openomsi/launcher.log.
fn log_line(line: &str) {
    use std::io::Write;
    let p = data_dir().join("launcher.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = writeln!(f, "{now} {line}");
    }
}

pub fn list_vehicles() -> Result<Vec<VehicleInfo>> {
    list_vehicles_progress(|_, _, _| {})
}

/// The buses, as `list_vehicles`, with `progress` told after every few folders what they
/// held, how many folders are done and how many there are: a big installation's first
/// reading (thousands of vehicle folders, nothing in the cache yet) takes minutes, and the
/// page showed nothing at all until the last folder was read.
pub fn list_vehicles_progress(progress: impl Fn(&[VehicleInfo], usize, usize)) -> Result<Vec<VehicleInfo>> {
    use rayon::prelude::*;
    root()?;
    let lang = content_language();
    let folders = merged_folders("Vehicles");
    let keys: Vec<String> = folders.iter().map(|(_, dirs)| format!("bus4|{lang}|{}", dirs.iter().map(|d| d.to_string_lossy()).collect::<Vec<_>>().join("|"))).collect();
    let read = |(folder, dirs): &(String, Vec<PathBuf>), key: &String| -> Vec<VehicleInfo> {
        // the stamp covers every copy of the folder and their direct entries (Model/,
        // Texture/ ...); the paint folders the entry read are its dependencies
        let mut stamped: Vec<PathBuf> = dirs.clone();
        for d in dirs {
            if let Some(list) = omsi_cfg::vfs::list_dir(d) {
                let mut subs: Vec<PathBuf> = list.into_iter().filter(|(_, is_dir)| *is_dir).map(|(n, _)| d.join(n)).collect();
                subs.sort();
                stamped.extend(subs);
            }
        }
        index::cached(key, index::folder_stamp(&stamped), || read_vehicle_folder(folder, dirs, lang))
    };
    // folders side by side (the files of one wait for the disk while another's are parsed),
    // a handful of threads so that a hard disk is not sent seeking all over
    let pool = rayon::ThreadPoolBuilder::new().num_threads(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8)).build().ok();
    let mut out = Vec::new();
    let total = folders.len();
    let mut done = 0;
    let mut saved = std::time::Instant::now();
    for (chunk, chunk_keys) in folders.chunks(32).zip(keys.chunks(32)) {
        let lists: Vec<Vec<VehicleInfo>> = match &pool {
            Some(pool) => pool.install(|| chunk.par_iter().zip(chunk_keys.par_iter()).map(|(f, k)| read(f, k)).collect()),
            None => chunk.iter().zip(chunk_keys.iter()).map(|(f, k)| read(f, k)).collect(),
        };
        let batch: Vec<VehicleInfo> = lists.into_iter().flatten().collect();
        done += chunk.len();
        // what was read is kept every few seconds: a first reading left half-way (the
        // launcher closed) starts from there the next time
        if saved.elapsed().as_secs() >= 10 {
            index::save("bus4|", None);
            saved = std::time::Instant::now();
        }
        progress(&batch, done, total);
        out.extend(batch);
    }
    index::save("bus4|", Some(&keys));
    if out.is_empty() {
        log_empty("Vehicles", ".bus file");
    }
    Ok(out)
}

/// The buses of one vehicle folder (all its copies; a bus file in the content folder hides
/// the one of the same name in the OMSI folder), and the folders read besides its own.
fn read_vehicle_folder(folder: &str, dirs: &[PathBuf], lang: &str) -> (Vec<VehicleInfo>, Vec<PathBuf>) {
    // Every file of the folder and of the folders in it, however deep: OMSI's bus list looks
    // for `*.bus` and `*.ovh` under Vehicles to any depth (Omsi.exe 0x67bdf9, the search
    // with depth 255) - `Vehicles\Pack\Variant\x.bus` was not listed here (#137). A file of
    // the content folder hides the one at the same place in the installation.
    fn walk(d: &Path, rel: &str, depth: u32, out: &mut Vec<(String, PathBuf)>) {
        let mut list = omsi_cfg::vfs::list_dir(d).unwrap_or_default();
        list.sort();
        for (n, is_dir) in list {
            let name = n.to_string_lossy().to_string();
            let r = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            if is_dir {
                if depth > 0 && !name.starts_with('.') {
                    walk(&d.join(&n), &r, depth - 1, out);
                }
            } else {
                out.push((r, d.join(&n)));
            }
        }
    }
    let mut files: Vec<PathBuf> = Vec::new();
    let mut rel_of: std::collections::HashMap<PathBuf, String> = std::collections::HashMap::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for d in dirs {
        let mut here = Vec::new();
        walk(d, "", 8, &mut here);
        for (r, f) in here {
            if seen.insert(r.to_ascii_lowercase()) {
                rel_of.insert(f.clone(), r);
                files.push(f);
            }
        }
    }
    let mut out = Vec::new();
    let mut deps = Vec::new();
    if !files.iter().any(|f| f.extension().map(|e| e.eq_ignore_ascii_case("bus") || e.eq_ignore_ascii_case("ovh")).unwrap_or(false)) {
        // textures, or a repaint for a bus that is not installed: nothing to drive
        log_line(&format!("vehicles: Vehicles/{folder} has no .bus file (a repaint or textures for a bus that is not installed?) - not listed"));
        return (out, deps);
    }
    // the depot files beside each bus (a bus in a folder of the pack: those of its folder,
    // else those of the pack's own)
    let hof_dir = |f: &PathBuf| rel_of.get(f).map(|r| r.rsplit_once('/').map(|(d, _)| d.to_ascii_lowercase()).unwrap_or_default()).unwrap_or_default();
    let mut hofs_in: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    for f in files.iter().filter(|f| f.extension().map(|e| e.eq_ignore_ascii_case("hof")).unwrap_or(false)) {
        // (the name alone: UK depot files carry megabytes of trips)
        if let Some(name) = omsi_vehicle::Hof::read_name(f) {
            hofs_in.entry(hof_dir(f)).or_default().push(name.trim().to_string());
        }
    }
    // OMSI offers what has a [friendlyname]: never the rear section of an articulated bus
    // (its front brings it along), an AI-only variant or a car
    for (f, v) in omsi_vehicle::vehicle::offered_vehicles(&files) {
        let f = &f;
        let stem = f.file_stem().unwrap().to_string_lossy().to_ascii_lowercase();
        // a vehicle file whose model is not there would load as nothing
        let model = v.model.as_ref().map(|m| omsi_cfg::resolve_path(v.dir(), m));
        if !model.as_ref().map(|m| omsi_cfg::vfs::is_file(m)).unwrap_or(false) {
            log_line(&format!("vehicles: {} - its model {} is missing, not listed", f.display(), model.map(|m| m.display().to_string()).unwrap_or_else(|| "(none)".into())));
            continue;
        }
        let rel = format!("Vehicles/{}/{}", folder, rel_of.get(f).cloned().unwrap_or_else(|| f.file_name().unwrap().to_string_lossy().to_string()));
        let hofs = hofs_in.get(&hof_dir(f)).or_else(|| hofs_in.get("")).cloned().unwrap_or_default();
        let name = format!("{} {}", v.manufacturer.trim(), v.type_name.trim()).trim().to_string();
        let (paints, paint_dirs) = paint_schemes(&v);
        deps.extend(paint_dirs);
        // `<bus>_ENG.dsc` beside the bus file (the language of the settings) describes it
        let description = find_dsc(f, lang).map(|x| x.description).filter(|t| !t.is_empty()).unwrap_or_else(|| v.description.trim().to_string());
        let missing_packs = model.as_deref().map(missing_packs_of).unwrap_or_default();
        if !missing_packs.is_empty() {
            log_line(&format!("vehicles: {} borrows parts from packs that are not installed: {}", f.display(), missing_packs.join(", ")));
        }
        out.push(VehicleInfo { name: if name.is_empty() { stem.clone() } else { name }, manufacturer: v.manufacturer.trim().to_string(), type_name: v.type_name.trim().to_string(), file: rel, folder: folder.to_string(), description: description.chars().take(600).collect(), default_paint: v.default_paint.trim().to_string(), paints, hofs, installed: in_content(f), missing_packs, numbers: v.numbers_with_plates() });
    }
    deps.sort();
    deps.dedup();
    // the folders themselves are stamped by the caller
    deps.retain(|d| !dirs.contains(d));
    (out, deps)
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WeatherInfo {
    pub name: String,
    pub file: String,
    pub description: String,
    pub fog_m: f32,
    pub temp: f32,
    pub clouds: String,
    pub precip: String,
    pub snow: bool,
    #[serde(default)]
    pub installed: bool,
}

pub fn list_weather() -> Result<Vec<WeatherInfo>> {
    root()?;
    let mut out = Vec::new();
    let mut keys = Vec::new();
    let lang = content_language();
    let mut files: Vec<PathBuf> = merged_entries("Weather").into_iter().filter(|p| p.extension().map(|e| e.eq_ignore_ascii_case("owt")).unwrap_or(false)).collect();
    files.sort();
    for f in files {
        // the description in the settings' language lives in `<name>_ENG.dsc`
        let mut stamped = vec![f.clone()];
        stamped.extend(dsc_candidates(&f, lang));
        let key = format!("wx|{lang}|{}", f.display());
        keys.push(key.clone());
        let info: Option<WeatherInfo> = index::cached(&key, index::files_stamp(&stamped), || (read_weather(&f, lang), Vec::new()));
        out.extend(info);
    }
    index::save("wx|", Some(&keys));
    Ok(out)
}

fn read_weather(f: &Path, lang: &str) -> Option<WeatherInfo> {
    let w = omsi_content::weather::Weather::load(f).ok()?;
    let stem = f.file_stem().unwrap().to_string_lossy().to_string();
    // (the `[name]` of the file is not part of the description: "Ground Fog Heavy ground fog ...")
    let description = find_dsc(f, lang).map(|x| x.description.replace('\n', " ")).filter(|t| !t.is_empty()).unwrap_or_else(|| w.description.replace('\n', " "));
    let precip = match w.precip.first().copied().unwrap_or(0.0) as i32 {
        1 => format!("rain {:.0}%", w.precip.get(1).copied().unwrap_or(0.0) / 255.0 * 100.0),
        2 => format!("snow {:.0}%", w.precip.get(1).copied().unwrap_or(0.0) / 255.0 * 100.0),
        _ => "dry".into(),
    };
    Some(WeatherInfo { name: stem.trim_start_matches('#').to_string(), file: format!("Weather/{}", f.file_name().unwrap().to_string_lossy()), description, fog_m: w.fog.0, temp: w.temp.0, clouds: if w.clouds.0.trim().starts_with("-1") { "clear".into() } else { w.clouds.0.trim().to_string() }, precip, snow: w.snow, installed: in_content(f) })
}

/// The content language of the settings (`language=`, English by default): which
/// `<file>_<LANG>.dsc` names and describes maps, buses and weathers.
fn content_language() -> &'static str {
    let text = std::fs::read_to_string(data_dir().join("settings.cfg")).ok();
    language_code(settings_from_text(text.as_deref())["language"].as_str().unwrap_or("ENG"))
}

/// The description files OMSI reads for `file` (`global.cfg` -> `global_ENG.dsc`), in the
/// order they are tried: the language itself, then English for a language that has none
/// (German is the files' own language, so German falls back to the file itself).
fn dsc_candidates(file: &Path, lang: &str) -> Vec<PathBuf> {
    let stem = file.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let mut langs = vec![lang];
    if lang != "DEU" && lang != "ENG" {
        langs.push("ENG");
    }
    langs.into_iter().map(|l| file.with_file_name(format!("{stem}_{l}.dsc"))).collect()
}

/// A `.dsc` file: the `[name]` / `[friendlyname]` lines and the `[description]` text.
struct Dsc {
    name: Vec<String>,
    description: String,
}

fn parse_dsc(text: &str) -> Dsc {
    let mut name = Vec::new();
    let mut desc: Vec<&str> = Vec::new();
    let mut section = "";
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') {
            section = if t.eq_ignore_ascii_case("[name]") || t.eq_ignore_ascii_case("[friendlyname]") {
                "name"
            } else if t.eq_ignore_ascii_case("[description]") {
                "description"
            } else {
                ""
            };
            continue;
        }
        match section {
            "name" if !t.is_empty() => name.push(t.to_string()),
            "name" => section = "",
            "description" => desc.push(line.trim_end()),
            _ => {}
        }
    }
    Dsc { name, description: desc.join("\n").trim().to_string() }
}

fn find_dsc(file: &Path, lang: &str) -> Option<Dsc> {
    dsc_candidates(file, lang).iter().find_map(|p| omsi_cfg::vfs::read(p).ok()).map(|b| parse_dsc(&encoding_latin1(&b)))
}

/// OMSI's text files are Latin-1, except that some description files were saved as
/// UTF-16 with a byte-order mark.
fn encoding_latin1(b: &[u8]) -> String {
    if b.len() >= 2 && b[0] == 0xFF && b[1] == 0xFE {
        let u: Vec<u16> = b[2..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        return String::from_utf16_lossy(&u);
    }
    if b.len() >= 2 && b[0] == 0xFE && b[1] == 0xFF {
        let u: Vec<u16> = b[2..].chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        return String::from_utf16_lossy(&u);
    }
    b.iter().map(|&c| c as char).collect()
}

// ---------------------------------------------------------------------------------------
// timetable: lines, tours, trips and the roadbook

#[derive(Serialize, Clone, Debug)]
pub struct StopInfo {
    pub name: String,
    pub arr: f64,
    pub dep: f64,
}

/// One trip of a tour, as OMSI's timetable dialog lists it: when it leaves, from where to
/// where, on which line, and the trip's (route's) own name.
#[derive(Serialize, Clone, Debug)]
pub struct TripInfo {
    /// The trip file's name ("4 Liman-ZS"): the name the map gives the route.
    pub name: String,
    /// Its place in the tour, 1 = the first (what `--trip` takes, as does its departure).
    pub index: usize,
    /// The line its displays show (a depot run has none).
    pub line: String,
    /// First and last stop.
    pub from: String,
    pub terminus: String,
    pub departure: f64,
    pub arrival: f64,
    pub stops: Vec<StopInfo>,
    pub km: f64,
}

#[derive(Serialize, Clone, Debug)]
pub struct TourInfo {
    pub number: String,
    pub ai_group: String,
    pub first: f64,
    pub last: f64,
    /// The days it runs, in words ("Mon-Fri", "Sat", "daily", ...), from its validity mask.
    pub days: String,
    /// It runs on the date asked for (the game's timetable has only these tours that day).
    pub runs: bool,
    /// The first date from the one asked for on which it runs (`YYYY-MM-DD`): the game's
    /// timetable dialog lists only the tours of the chosen day, so a
    /// tour of another day is picked by moving the date to it.
    pub next_run: Option<String>,
    pub trips: Vec<TripInfo>,
}

#[derive(Serialize, Clone, Debug)]
pub struct LineInfo {
    pub name: String,
    pub user_allowed: bool,
    pub termini: Vec<String>,
    pub tours: Vec<TourInfo>,
}

/// The date the game starts on without `--date` (its clock's default, day 150 of 1989), and
/// the launcher's own default date.
pub const DEFAULT_DATE: &str = "1989-05-30";

/// A trip as a map picture needs it: the map's own road pieces the trip drives, in order,
/// and its stops.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TripPath {
    /// The road pieces it drives, in order (the shared piece of two links appears once).
    pub route: Vec<RoadPiece>,
    /// The stop objects the trip calls at, in order.
    pub stops: Vec<i64>,
}

/// One spline of the map a trip drives over: its id in its tile, which `[path]` of it the
/// trip uses, and the tile's place in `global.cfg`'s `[map]` list. It is the game's own
/// `LaneKey` (`schedule::steps_of` builds the same three fields), so the launcher's map
/// picture can find the very lane the game would drive the trip on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RoadPiece {
    pub tile_x: i32,
    pub tile_y: i32,
    pub spline: i64,
    /// The `[path]` of that spline's `.sli` the trip drives (the game's own lane).
    pub path: u16,
}

/// The route of trip `trip` of the map `map` on `date`: its road pieces and its stops.
/// The trip's own track (`.ttr`) when it names one - trains, ferries, and the type-1 trips
/// of mod maps - else the station links between its stops, as the game walks them.
pub fn trip_path(map: &str, date: &str, trip: &str) -> Result<TripPath> {
    let map_dir = resolve_content(map)?.parent().map(|p| p.to_path_buf()).context("map folder")?;
    let date = if date.trim().is_empty() { DEFAULT_DATE } else { date.trim() };
    let code = omsi_map::date_code(date).with_context(|| format!("'{date}' is not a date (YYYY-MM-DD)"))?;
    let chrono = omsi_map::active_chrono_dirs(&map_dir, code);
    let off = omsi_map::chrono_deactivated_lines(&chrono);
    let data = omsi_timetable::TimetableData::load_with_chrono(&map_dir, &chrono, &off);
    let mut out = TripPath::default();
    let Some(t) = data.trips.iter().find(|x| x.name.eq_ignore_ascii_case(trip.trim())) else { return Ok(out) };
    // a type-1 trip's stations are its own `[station]` records (all of Novi Sad)
    out.stops = if t.stations.is_empty() {
        t.stations_legacy.iter().filter_map(|r| r.first()?.trim().parse::<i64>().ok()).collect()
    } else {
        t.stations.clone()
    };
    // the tile of a path step is the index of its entry in `[map]`, as the game reads it
    let tiles = omsi_map::GlobalCfg::load(&map_dir.join("global.cfg")).map(|g| g.raw_tiles).unwrap_or_default();
    let at = |tile_index: f64| tiles.get(tile_index as usize).copied();
    let track_name = if t.display_name.trim().is_empty() { t.name.trim() } else { t.display_name.trim() };
    if let Some(track) = data.tracks.iter().find(|x| x.path.file_stem().map(|s| s.to_string_lossy().eq_ignore_ascii_case(track_name)).unwrap_or(false)) {
        for e in &track.entries {
            if e.values.len() < 5 {
                continue;
            }
            if let Some(tile) = at(e.values[2]) {
                out.route.push(RoadPiece { tile_x: tile.0, tile_y: tile.1, spline: e.values[0] as i64, path: e.values[1] as u16 });
            }
        }
        return Ok(out);
    }
    for w in out.stops.windows(2) {
        let Some(link) = data.stn_links.iter().find(|l| l.from_id == w[0] && l.to_id == w[1]) else { continue };
        for e in &link.entries {
            if e.values.len() < 4 {
                continue;
            }
            let Some(tile) = at(e.values[2]) else { continue };
            let p = RoadPiece { tile_x: tile.0, tile_y: tile.1, spline: e.values[0] as i64, path: e.values[1] as u16 };
            // consecutive links repeat the piece they share
            if out.route.last() != Some(&p) {
                out.route.push(p);
            }
        }
    }
    Ok(out)
}

/// The lines of a map's timetable on `date` (`YYYY-MM-DD`, the game's default when empty):
/// the chrono folders active that day add their lines and take theirs off, as the game does
/// - Spandau's 1991 timetable change replaces line "5 & 5N" and sixteen others.
pub fn list_lines(map: &str, date: &str) -> Result<Vec<LineInfo>> {
    let map_dir = resolve_content(map)?.parent().map(|p| p.to_path_buf()).context("map folder")?;
    lines_on(&map_dir, date)
}

fn lines_on(map_dir: &Path, date: &str) -> Result<Vec<LineInfo>> {
    let date = if date.trim().is_empty() { DEFAULT_DATE } else { date.trim() };
    let code = omsi_map::date_code(date).with_context(|| format!("'{date}' is not a date (YYYY-MM-DD)"))?;
    let chrono = omsi_map::active_chrono_dirs(map_dir, code);
    let off = omsi_map::chrono_deactivated_lines(&chrono);
    let data = omsi_timetable::TimetableData::load_with_chrono(map_dir, &chrono, &off);
    // which tours run that day: the tour's mask as the game reads it (bits 0-6 Monday to
    // Sunday, 7 a public holiday, 8 school holidays, 9 school days: the original)
    let calendar = omsi_map::Calendar::load(&map_dir.join("Holidays.txt")).unwrap_or_default();
    let day_bit = if calendar.is_holiday(code) { 1 << 7 } else { 1 << weekday(code) };
    let school_bit = if calendar.in_holiday_range(code) { 1 << 8 } else { 1 << 9 };
    let mut out = Vec::new();
    for l in &data.lines {
        let mut termini: Vec<String> = Vec::new();
        let mut tours = Vec::new();
        for t in &l.tours {
            let mask = t.extra.trim().parse::<i32>().unwrap_or(1023);
            let runs_on = |c: i32| {
                let day = if calendar.is_holiday(c) { 1 << 7 } else { 1 << weekday(c) };
                let school = if calendar.in_holiday_range(c) { 1 << 8 } else { 1 << 9 };
                mask & day != 0 && mask & school != 0
            };
            let runs = mask & day_bit != 0 && mask & school_bit != 0;
            let next_run = (0..400).map(|k| add_days(code, k)).find(|c| runs_on(*c)).map(|c| format!("{:04}-{:02}-{:02}", c / 10000, c / 100 % 100, c % 100));
            let mut trips = Vec::new();
            for tt in &t.trips {
                let Some(trip) = data.trips.iter().find(|x| x.name.eq_ignore_ascii_case(&tt.trip)) else { continue };
                let departure = tt.departure as f64 * 60.0;
                let duration = trip.profiles.get(tt.profile.max(0) as usize).or(trip.profiles.first()).map(|p| p.factor as f64 * 60.0).filter(|d| *d > 0.0).unwrap_or(600.0);
                // its stations: [station_typ2] objects, or the [station] records of a type-1
                // trip (all of Novi Sad), which carry their stop's name themselves
                let legacy: Vec<(i64, String)> = trip
                    .stations_legacy
                    .iter()
                    .filter_map(|r| Some((r.first()?.trim().parse::<i64>().ok()?, r.get(2).map(|n| n.trim().to_string()).unwrap_or_default())))
                    .collect();
                let stations: Vec<i64> = if trip.stations.is_empty() { legacy.iter().map(|x| x.0).collect() } else { trip.stations.clone() };
                let mut lens = Vec::new();
                for w in stations.windows(2) {
                    lens.push(data.stn_links.iter().find(|k| k.from_id == w[0] && k.to_id == w[1]).map(|k| k.length.max(1.0)).unwrap_or(500.0));
                }
                let total: f64 = lens.iter().sum::<f64>().max(1.0);
                let mut acc = 0.0;
                let mut stops = Vec::new();
                for (i, id) in stations.iter().enumerate() {
                    let t_at = departure + duration * acc / total;
                    let name = data
                        .bus_stops
                        .iter()
                        .find(|b| b.object_id == *id)
                        .map(|b| b.name.trim().to_string())
                        .or_else(|| legacy.iter().find(|x| x.0 == *id).map(|x| x.1.clone()).filter(|n| !n.is_empty()))
                        .unwrap_or_else(|| format!("stop {id}"));
                    stops.push(StopInfo { name, arr: t_at, dep: if i == 0 { departure } else { t_at } });
                    if i < lens.len() {
                        acc += lens[i];
                    }
                }
                if !trip.terminus.trim().is_empty() && !termini.iter().any(|x| x == trip.terminus.trim()) {
                    termini.push(trip.terminus.trim().to_string());
                }
                let from = stops.first().map(|s| s.name.clone()).unwrap_or_default();
                let index = trips.len() + 1;
                trips.push(TripInfo { name: trip.name.clone(), index, line: trip.line.trim().to_string(), from, terminus: trip.terminus.trim().to_string(), departure, arrival: departure + duration, stops, km: total / 1000.0 });
            }
            // (a tour with no trip the timetable knows cannot be driven: not offered)
            if trips.is_empty() {
                continue;
            }
            let first = trips.first().map(|t| t.departure).unwrap_or(0.0);
            let last = trips.last().map(|t| t.arrival).unwrap_or(0.0);
            tours.push(TourInfo { number: t.number.clone(), ai_group: t.ai_group.clone(), first, last, days: days_of(mask), runs, next_run, trips });
        }
        tours.sort_by(|a, b| a.first.total_cmp(&b.first));
        out.push(LineInfo { name: l.name.clone(), user_allowed: l.user_allowed, termini, tours });
    }
    out.sort_by(|a, b| natural_key(&a.name).cmp(&natural_key(&b.name)));
    Ok(out)
}

/// A date code (YYYYMMDD) `k` days later.
fn add_days(code: i32, k: i32) -> i32 {
    let (mut y, mut m, mut d) = (code / 10000, code / 100 % 100, code % 100 + k);
    let len = |y: i32, m: i32| match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    while d > len(y, m) {
        d -= len(y, m);
        m += 1;
        if m > 12 {
            m = 1;
            y += 1;
        }
    }
    y * 10000 + m * 100 + d
}

/// Day of the week of a date code (YYYYMMDD): 0 = Monday … 6 = Sunday, as the game's clock.
fn weekday(code: i32) -> i32 {
    let (mut y, m, d) = (code / 10000, code / 100 % 100, code % 100);
    // Sakamoto's method (0 = Sunday)
    let t = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    if m < 3 {
        y -= 1;
    }
    let sunday0 = (y + y / 4 - y / 100 + y / 400 + t[((m - 1).clamp(0, 11)) as usize] + d).rem_euclid(7);
    (sunday0 + 6) % 7
}

/// A tour's validity mask in words: "Mon-Fri", "Sat", "Sun", "daily", ...
fn days_of(mask: i32) -> String {
    let week = mask & 0x7f;
    let names = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let mut out = match week {
        0x7f => "daily".to_string(),
        0x1f => "Mon-Fri".to_string(),
        0x3f => "Mon-Sat".to_string(),
        0x60 => "Sat-Sun".to_string(),
        0 => String::new(),
        w => (0..7).filter(|i| w & (1 << i) != 0).map(|i| names[i]).collect::<Vec<_>>().join(", "),
    };
    if mask & (1 << 7) != 0 && week != 0x7f {
        out.push_str(if out.is_empty() { "holidays" } else { " & holidays" });
    }
    match (mask & (1 << 8) != 0, mask & (1 << 9) != 0) {
        (true, false) => out.push_str(", school holidays"),
        (false, true) => out.push_str(", school days"),
        _ => {}
    }
    out
}

fn natural_key(s: &str) -> (u64, String) {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    (digits.parse().unwrap_or(u64::MAX), s.to_ascii_lowercase())
}

/// What the driver types into the IBIS for a line, from the bus's depot file: the line
/// code, and for each destination the route code and the terminus code.
#[derive(Serialize, Clone, Debug)]
pub struct IbisInfo {
    pub hof: String,
    pub line_code: String,
    pub routes: Vec<IbisRoute>,
}

#[derive(Serialize, Clone, Debug)]
pub struct IbisRoute {
    pub code: String,
    pub route: String,
    pub name: String,
    pub terminus_code: i32,
    pub terminus: String,
}

pub fn ibis_info(bus: &str, hof_name: &str, line: &str) -> Result<IbisInfo> {
    let bus_path = resolve_content(bus)?;
    let dir = bus_path.parent().context("bus folder")?;
    // the bus's own depot file of that name (every content root's copy of its folder), else
    // the one another bus brings (the game borrows it the same way), else the bus's first
    let hof = omsi_vehicle::hof::depot_in(dir, hof_name)
        .or_else(|| omsi_vehicle::hof::depot_anywhere(hof_name))
        .or_else(|| omsi_vehicle::hof::depot_files(dir).iter().find_map(|f| omsi_vehicle::Hof::load(f).ok()))
        .context("no depot file next to the bus")?;
    let hof = &hof;
    let line_digits: String = line.chars().take_while(|c| c.is_ascii_digit()).collect();
    let mut routes = Vec::new();
    for t in &hof.info_trips {
        let matches = t.line.trim().eq_ignore_ascii_case(line.trim()) || (!line_digits.is_empty() && t.code.trim_start_matches('0').starts_with(line_digits.trim_start_matches('0')) && t.code.len() >= line_digits.len());
        if !matches {
            continue;
        }
        let code = omsi_cfg::parse_i32(&t.route);
        let terminus = hof.termini.iter().find(|x| x.code == code).and_then(|x| x.strings.first().cloned()).unwrap_or_default();
        routes.push(IbisRoute { code: t.code.clone(), route: t.route.clone(), name: t.name.clone(), terminus_code: code, terminus });
    }
    Ok(IbisInfo { hof: hof.name.clone(), line_code: line_digits, routes })
}

// ---------------------------------------------------------------------------------------
// profiles: the driver's personnel file plus the sessions the game writes

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct Session {
    pub time: u64,
    pub driver: String,
    pub map: String,
    pub bus: String,
    pub line: Option<String>,
    pub tour: Option<String>,
    pub seconds: f64,
    pub metres: f64,
    pub stops: i32,
    pub early: i32,
    pub late: i32,
    pub tickets: i32,
    pub cash: f64,
    pub crashes: i32,
    pub hurt: i32,
    pub jolts: i32,
    pub driving: f64,
    pub comfort: f64,
    pub ticketing: f64,
}

#[derive(Serialize, Clone, Debug)]
pub struct Profile {
    pub name: String,
    pub file: String,
    pub hours: f64,
    pub km: f64,
    pub xp: i64,
    pub level: i64,
    pub next_level_xp: i64,
    pub stops: i32,
    pub early: i32,
    pub late: i32,
    pub tickets: f64,
    pub cash: f64,
    pub crashes: i32,
    pub hurt: i32,
    pub rating_driving: f64,
    pub rating_comfort: f64,
    pub rating_tickets: f64,
    pub sessions: Vec<Session>,
    pub exists: bool,
}

fn sessions() -> Vec<Session> {
    let dir = data_dir().join("sessions");
    let mut out: Vec<Session> = std::fs::read_dir(&dir).map(|rd| rd.flatten().filter_map(|e| std::fs::read_to_string(e.path()).ok()).filter_map(|t| serde_json::from_str::<Session>(&t).ok()).collect()).unwrap_or_default();
    out.sort_by(|a, b| b.time.cmp(&a.time));
    out
}

/// Experience: a point per hundred metres, five per stop served on time, two per ticket,
/// minus twenty per crash and fifty per pedestrian; the level grows with the square root.
fn xp_of(s: &Session) -> i64 {
    let on_time = (s.stops - s.early - s.late).max(0) as i64;
    let xp = (s.metres / 100.0) as i64 + on_time * 5 + s.tickets as i64 * 2 + (s.seconds / 60.0) as i64 - s.crashes as i64 * 20 - s.hurt as i64 * 50 - s.jolts as i64;
    xp.max(0)
}

fn level_of(xp: i64) -> (i64, i64) {
    let level = ((xp as f64 / 250.0).sqrt().floor() as i64) + 1;
    let next = (level * level) as i64 * 250;
    (level, next)
}

/// Where a driver's personnel file is written: the content folder's `Drivers` (the
/// original installation is only ever read).
fn driver_write_path(root: &Path, name: &str) -> PathBuf {
    content_dir().unwrap_or_else(|| root.to_path_buf()).join("Drivers").join(format!("{name}.odr"))
}

/// Where a driver's personnel file is read: the content folder's copy once the game has
/// written one, else the original installation's (the stock `OMSI-Fan.odr`).
fn driver_read_path(root: &Path, name: &str) -> PathBuf {
    let own = driver_write_path(root, name);
    if own.exists() {
        own
    } else {
        root.join("Drivers").join(format!("{name}.odr"))
    }
}

pub fn list_profiles() -> Result<Vec<String>> {
    let root = root()?;
    let odr_names = |dir: PathBuf| -> Vec<String> { std::fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().map(|e| e.eq_ignore_ascii_case("odr")).unwrap_or(false)).filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().to_string())).collect()).unwrap_or_default() };
    let mut names = odr_names(root.join("Drivers"));
    if let Some(c) = content_dir() {
        names.extend(odr_names(c.join("Drivers")));
    }
    for s in sessions() {
        if !names.iter().any(|n| n.eq_ignore_ascii_case(&s.driver)) {
            names.push(s.driver.clone());
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

pub fn get_profile(name: &str) -> Result<Profile> {
    let root = root()?;
    let name = name.trim();
    if name.is_empty() {
        return Err(anyhow!("a profile needs a name"));
    }
    let file = driver_read_path(&root, name);
    let driver = omsi_content::driver::Driver::load(&file).ok();
    let mine: Vec<Session> = sessions().into_iter().filter(|s| s.driver.eq_ignore_ascii_case(name)).collect();
    let xp: i64 = mine.iter().map(xp_of).sum();
    let (level, next) = level_of(xp);
    let hours = mine.iter().map(|s| s.seconds).sum::<f64>() / 3600.0;
    let (km, stops, early, late, tickets, cash, crashes, hurt, rating) = match &driver {
        Some(d) => (d.hektom / 10.0, d.bus_stops[0], d.bus_stops[1], d.bus_stops[2], d.tickets[0], d.tickets[1], d.crashes[0], d.crashes[1], [d.driving_percent(), d.comfort_percent().unwrap_or(100.0), d.ticket_percent().unwrap_or(100.0)]),
        None => {
            let km = mine.iter().map(|s| s.metres).sum::<f64>() / 1000.0;
            let n = mine.len().max(1) as f64;
            (km, mine.iter().map(|s| s.stops).sum(), mine.iter().map(|s| s.early).sum(), mine.iter().map(|s| s.late).sum(), mine.iter().map(|s| s.tickets as f64).sum(), mine.iter().map(|s| s.cash).sum(), mine.iter().map(|s| s.crashes).sum(), mine.iter().map(|s| s.hurt).sum(), [mine.iter().map(|s| s.driving).sum::<f64>() / n, mine.iter().map(|s| s.comfort).sum::<f64>() / n, mine.iter().map(|s| s.ticketing).sum::<f64>() / n])
        }
    };
    Ok(Profile { name: name.to_string(), file: format!("Drivers/{name}.odr"), hours, km, xp, level, next_level_xp: next, stops, early, late, tickets, cash, crashes, hurt, rating_driving: rating[0], rating_comfort: rating[1], rating_tickets: rating[2], sessions: mine.into_iter().take(40).collect(), exists: driver.is_some() })
}

/// Create the personnel file for a new driver (OMSI's own `.odr` format), so that the game
/// can add every run to it.
pub fn create_profile(name: &str, sex: &str) -> Result<Profile> {
    let root = root()?;
    let name = name.trim();
    if name.is_empty() || name.contains(['/', '\\', ':']) {
        return Err(anyhow!("the name must be a plain file name"));
    }
    let file = driver_write_path(&root, name);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if !file.exists() && !driver_read_path(&root, name).exists() {
        let d = omsi_content::driver::Driver { path: file.clone(), name: name.to_string(), sex: if sex.trim().is_empty() { "M".into() } else { sex.trim().to_string() }, ..Default::default() };
        d.save(&file)?;
    }
    let mut c = load_config();
    c.profile = name.to_string();
    save_config(&c)?;
    get_profile(name)
}

/// Delete a driver's personnel file from the content folder. A driver who exists only in
/// the original installation is left there: the original is never written.
pub fn delete_profile(name: &str) -> Result<()> {
    let root = root()?;
    let file = driver_write_path(&root, name.trim());
    if file.exists() {
        std::fs::remove_file(&file)?;
    } else if root.join("Drivers").join(format!("{}.odr", name.trim())).exists() {
        return Err(anyhow!("'{}' belongs to the original OMSI 2 installation, which is not changed", name.trim()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// key bindings: the content folder's Inputs/keyboard.cfg once the page has saved one (the
// game reads that first), else the original installation's, which is never written

fn keyboard_cfg_write_path() -> Result<PathBuf> {
    let cand = content_dir().unwrap_or(root()?).join("Inputs").join("keyboard.cfg");
    if let Some(p) = cand.parent() {
        if (p.exists() || std::fs::create_dir_all(p).is_ok()) && omsi_cfg::is_writable(p) {
            return Ok(cand);
        }
    }
    let fallback = data_dir().join("Inputs").join("keyboard.cfg");
    if let Some(p) = fallback.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    Ok(fallback)
}

fn keyboard_cfg_read_path() -> Result<PathBuf> {
    let own = keyboard_cfg_write_path()?;
    if own.exists() {
        return Ok(own);
    }
    let fallback = data_dir().join("Inputs").join("keyboard.cfg");
    if fallback.exists() {
        return Ok(fallback);
    }
    Ok(omsi_cfg::original_keyboard_cfg(&root()?))
}

fn binding_to_json(b: &omsi_content::input::KeyBinding) -> Value {
    json!({ "action": b.action, "scan_code": b.scan_code, "modifier": b.modifier })
}

fn binding_from_json(v: &Value) -> Option<omsi_content::input::KeyBinding> {
    Some(omsi_content::input::KeyBinding {
        action: v.get("action")?.as_str()?.to_string(),
        scan_code: v.get("scan_code")?.as_i64()? as i32,
        modifier: v.get("modifier").and_then(|x| x.as_i64()).unwrap_or(0) as i32,
    })
}

pub fn get_keybindings() -> Result<Value> {
    let path = keyboard_cfg_read_path()?;
    let k = omsi_content::input::KeyboardCfg::load(&path)?.with_game_defaults().with_vr_defaults();
    Ok(json!({ "game": k.game.iter().map(binding_to_json).collect::<Vec<_>>(), "vehicles": k.vehicles.iter().map(binding_to_json).collect::<Vec<_>>() }))
}

/// Replace the bindings with the page's list. Written to a temp file and read back through
/// the same loader the game uses before it replaces the real file, so a page bug never
/// leaves the player with a `keyboard.cfg` the game itself cannot parse.
pub fn save_keybindings(v: &Value) -> Result<()> {
    let list = |k: &str| -> Vec<omsi_content::input::KeyBinding> { v.get(k).and_then(|x| x.as_array()).map(|a| a.iter().filter_map(binding_from_json).collect()).unwrap_or_default() };
    let k = omsi_content::input::KeyboardCfg { game: list("game"), vehicles: list("vehicles") };
    let path = keyboard_cfg_write_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("cfg.tmp");
    k.save(&tmp)?;
    omsi_content::input::KeyboardCfg::load(&tmp)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

// ---------------------------------------------------------------------------------------
// settings (the game's ~/.openomsi/settings.cfg)

pub fn get_settings() -> Result<Value> {
    let text = std::fs::read_to_string(data_dir().join("settings.cfg")).ok();
    let mut v = settings_from_text(text.as_deref());
    // no settings of our own yet: start from what the player set in OMSI 2
    if text.is_none() {
        if let Some(o) = root().ok().and_then(|r| omsi_options(&r)) {
            if let (Some(dst), Some(src)) = (v.as_object_mut(), o.settings.as_object()) {
                for (k, x) in src {
                    dst.insert(k.clone(), x.clone());
                }
            }
            v["imported_from_omsi"] = json!(true);
        }
    }
    // what "automatic" texture memory is on this machine (the game takes an eighth of it)
    v["texture_memory_auto"] = json!(physical_memory().map(|m| m / 8 / 1_000_000).unwrap_or(2000));
    Ok(v)
}

/// Other spellings the game reads for a key the launcher writes: they are read here too and
/// dropped when the file is written again (the game takes the last line of them, so a kept
/// `texmemlimit=` would undo the page's `texture_memory=`).
// (`navigator_opacity`: the opacity was the navigator's before it was the whole interface's)
const SETTING_ALIASES: &[(&str, &str)] = &[("af", "anisotropy"), ("ambient_occlusion", "ssao"), ("fractal", "detail_textures"), ("lang", "language"), ("texmemlimit", "texture_memory"), ("navigator_opacity", "ui_opacity"), ("gear_buttons_hold", "momentary_gears")];

/// A window size as the settings keep it: "WxH" in pixels (each 320..16384), else "auto".
pub fn resolution_text(v: &str) -> String {
    let v = v.trim().to_ascii_lowercase().replace(' ', "").replace(['*', '×'], "x");
    match v.split_once('x').map(|(w, h)| (w.trim().parse::<u32>(), h.trim().parse::<u32>())) {
        Some((Ok(w), Ok(h))) if (320..=16384).contains(&w) && (240..=16384).contains(&h) => format!("{w}x{h}"),
        _ => "auto".into(),
    }
}

fn setting_key(k: &str) -> String {
    let k = k.trim().to_ascii_lowercase();
    SETTING_ALIASES.iter().find(|(alias, _)| *alias == k).map(|(_, key)| key.to_string()).unwrap_or(k)
}

/// The interface's languages: the settings' code (OMSI's three-letter style), the name in
/// the language itself, the interface tables' code, and other spellings a file may use.
/// OMSI's own texts (key names, `.dsc` descriptions, tutorials) exist in English, German
/// and French: every other language shows those in English.
pub const LANGUAGES: &[(&str, &str, &str, &[&str])] = &[
    ("ENG", "English", "en", &["en", "english"]),
    ("DEU", "Deutsch", "de", &["de", "ger", "german", "deutsch"]),
    ("FRA", "Français", "fr", &["fr", "fre", "french", "francais", "français"]),
    ("RUS", "Русский", "ru", &["ru", "russian", "русский"]),
    ("UKR", "Українська", "uk", &["uk", "ua", "ukrainian", "українська"]),
    ("BEL", "Беларуская", "be", &["be", "by", "belarusian", "беларуская"]),
    ("KAZ", "Қазақша", "kk", &["kk", "kz", "kazakh", "қазақша"]),
    ("POL", "Polski", "pl", &["pl", "polish", "polski"]),
    ("CZE", "Čeština", "cs", &["cs", "cz", "czech", "čeština", "ces"]),
    ("HUN", "Magyar", "hu", &["hu", "hungarian", "magyar"]),
    ("ESP", "Español", "es", &["es", "spa", "spanish", "español"]),
    ("PTB", "Português (Brasil)", "pt", &["pt", "br", "pt-br", "por", "portuguese", "português"]),
    ("PTP", "Português (Portugal)", "pt-pt", &["pt-pt", "pt_pt", "pt-portugal", "portuguese-portugal", "português (portugal)", "português de portugal"]),
    ("ITA", "Italiano", "it", &["it", "italian", "italiano"]),
    ("NLD", "Nederlands", "nl", &["nl", "dutch", "nederlands"]),
    ("TUR", "Türkçe", "tr", &["tr", "turkish", "türkçe"]),
    ("JPN", "日本語", "ja", &["ja", "jp", "japanese", "日本語"]),
    ("ZHT", "繁體中文", "zh-tw", &["zh-tw", "zh-hant", "zh-hk", "zh-mo", "cht", "traditional chinese", "繁體中文", "繁体中文"]),
    ("KOR", "한국어", "ko", &["ko", "kr", "korean", "한국어"]),
    ("THA", "ไทย", "th", &["th", "thai", "ไทย"]),
    ("VIE", "Tiếng Việt", "vi", &["vi", "vietnamese", "tiếng việt"]),
    ("IND", "Bahasa Indonesia", "id", &["id", "indonesian", "bahasa indonesia"]),
    ("MSA", "Bahasa Melayu", "ms", &["ms", "malay", "bahasa melayu"]),
    ("TGL", "Filipino", "tl", &["tl", "fil", "filipino", "tagalog"]),
    ("CHS", "中文 (简体)", "zh", &["zh", "zh-cn", "zh-sg", "zh-hans", "zhs", "chs", "cn", "chinese", "simplified chinese", "简体中文", "簡體中文", "中文"]),
    ("HIN", "हिन्दी", "hi", &["hi", "hindi", "हिन्दी"]),
];

/// The settings' language code from any spelling the game accepts (English when unknown).
pub fn language_code(s: &str) -> &'static str {
    let s = s.trim().to_lowercase();
    LANGUAGES
        .iter()
        .find(|(code, _, _, aliases)| code.eq_ignore_ascii_case(&s) || aliases.iter().any(|a| *a == s))
        .map(|l| l.0)
        .unwrap_or("ENG")
}

/// The interface tables' code of a language (`ru`, `ja` ...; empty for English).
pub fn language_iso(code: &str) -> &'static str {
    let c = language_code(code);
    LANGUAGES.iter().find(|l| l.0 == c).map(|l| if l.2 == "en" { "" } else { l.2 }).unwrap_or("")
}

/// `vanilla` (as OMSI 2), `vanilla_plus` or `enhanced`, from the ways a file may spell them
/// (as the game's `settings::graphics_mode`).
pub fn graphics_mode(v: &str) -> &'static str {
    match v.trim().to_ascii_lowercase().replace(['-', ' '], "_").as_str() {
        "enhanced" | "1" => "enhanced",
        "vanilla" | "classic" | "original" | "omsi" | "omsi2" | "omsi_2" => "vanilla",
        _ => "vanilla_plus",
    }
}

/// How often the mirrors are drawn: `off`, `eco` or `full`, also from OMSI's
/// `performance_realreflexions` (none, economy, full).
fn mirror_refresh(x: &str) -> &'static str {
    match x.trim().to_ascii_lowercase().as_str() {
        "off" | "none" => "off",
        "eco" | "economy" => "eco",
        _ => "full",
    }
}

/// The page's view of a `settings.cfg` text (None: no file yet, the game's defaults).
pub fn settings_from_text(text: Option<&str>) -> Value {
    let mut v = json!({ "msaa": 4, "anisotropy": 8, "ssao": true, "shadows": true, "shadow_size": 2048, "navigator": true, "ui_opacity": 0.85, "navigator_corner": "bottom-left", "boarding": "auto", "detail_textures": true, "exact_fare": true, "enhanced": false, "graphics": "vanilla_plus", "fullscreen": false, "vsync": true, "volume": 0.6, "drive_keys": "simple", "render_scale": "auto", "view_distance": "auto", "language": "ENG", "texture_memory": 0, "texture_compression": true, "chat": true, "tooltips": true, "name_tags": true, "show_fps": false, "clouds": true, "pax_density": 1.0, "vol_ai": 1.0, "vol_scenery": 1.0, "mirror_size": 256, "doppler": true, "driver": true, "max_fps": 0, "min_obj_size": 0.013, "max_obj_dist": "auto" });
    v["triple_screen"] = json!(false);
    v["triple_span"] = json!(true);
    v["triple_hud_center"] = json!(true);
    v["triple_fov_deg"] = json!(0.0);
    v["triple_width_mm"] = json!(600);
    v["triple_distance_mm"] = json!(650);
    v["triple_bezel_mm"] = json!(0);
    v["triple_left_angle_deg"] = json!(45);
    v["triple_right_angle_deg"] = json!(45);
    v["triple_eye_height_mm"] = json!(0);
    v["vr"] = json!(false);
    v["vr_scale"] = json!(0.65);
    v["vr_head_smoothing_ms"] = json!(0);
    v["vr_mirror_rate"] = json!(16);
    v["mirror_refresh"] = json!("full");
    v["vr_desktop_mirror"] = json!(true);
    v["discord_status"] = json!(true);
    v["discord_app_id"] = json!("");
    // positional voice through GreenTeaSpeak's openOMSI plugin in multiplayer
    v["voice_chat"] = json!(true);
    // the launcher gives the graphics card up while a game runs (off: it stays drawn)
    v["launcher_rest"] = json!(true);
    // the window's size in pixels, "auto" to fit the screen (#904)
    v["resolution"] = json!("auto");
    // OMSI's own options
    for (k, d) in [("maintenance", json!(0)), ("ai_unsched_factor", json!(100)), ("ai_max_scheduled", json!(0)), ("ai_max_parked", json!(0)), ("ai_max_humans", json!(200)), ("use_real_time", json!(false)), ("use_real_date", json!(false)), ("use_real_year", json!(false)), ("collision_vehicles", json!(true)), ("collision_objects", json!(true)), ("collision_pedestrians", json!(true)), ("head_movement", json!(true)), ("driverview_smooth", json!(true)), ("hands_in_cab", json!(false)), ("alt_view", json!(true))] {
        v[k] = d;
    }
    // openOMSI's own: what passengers say, OMSI's route arrows, getting up from the seat
    for (k, d) in [("pax_voices", json!("all")), ("nav_arrows", json!(false)), ("nav_ai", json!(true)), ("get_up", json!(false)), ("time_speed", json!("1")), ("time_sync", json!(false)), ("metar_sync", json!(false)), ("metar_station", json!("")), ("machine_translation", json!(false)), ("shadow_casters", json!("all")), ("shadow_blobs", json!(true)), ("reflections", json!(true)), ("mouse_sens", json!(1.0)), ("graphics_api", json!("auto")), ("ctrl_off", json!("")), ("steering_linear", json!(false)), ("old_steering", json!(false)), ("red_steer_spd", json!(false)), ("ff_invert", json!(false)), ("ff_enabled", json!(true)), ("brake_hold", json!(true)), ("auto_clutch", json!(true)), ("momentary_gears", json!(false)), ("wheel_range", json!(900.0)), ("wheel_lock", json!(0.0)), ("fov", json!(0.0)), ("camera_collision", json!(true)), ("steer_look", json!(false)), ("pedal_throttle", json!(1.0)), ("pedal_brake", json!(1.0)), ("seat_x", json!(0.0)), ("seat_y", json!(0.0)), ("seat_z", json!(0.0)), ("head_tracking", json!(false)), ("led_glow", json!(6)), ("led_mips", json!(1.3)), ("ui_scale", json!(1.0)), ("ui_scale_window", json!(true)), ("notes", json!(true)), ("mouse_steering", json!(false)), ("mouse_right_off", json!(false)), ("blinker_cancel", json!(true))] {
        v[k] = d;
    }
    v["steer_look_angle"] = json!(30.0);
    v["look_sens"] = json!(1.0);
    v["steer_look_response"] = json!(0.25);
    v["head_idle"] = json!(0.0);
    v["head_idle_pace"] = json!(1.0);
    // updates from the GitHub releases: looked for when the launcher starts, installed
    // after asking (or at once)
    for (k, d) in [("update_check", json!(true)), ("update_auto", json!(false))] {
        v[k] = d;
    }
    let Some(t) = text else { return v };
    let mut version = 0;
    let mut graphics: Option<&str> = None;
    for line in t.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((k, val)) = line.split_once('=') else { continue };
        let (k, val) = (setting_key(k), val.trim());
        let b = |x: &str| matches!(x.to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes");
        match k.as_str() {
            "anisotropy" => v[&k] = json!(val.parse::<i64>().unwrap_or(8).clamp(1, 16)),
            "msaa" | "shadow_size" => v[&k] = json!(val.parse::<i64>().unwrap_or(0)),
            "ui_opacity" | "volume" | "vol_ai" | "vol_scenery" | "min_obj_size" => v[&k] = json!(val.parse::<f64>().unwrap_or(0.0)),
            "pax_density" => v[&k] = json!(val.trim_end_matches('%').parse::<f64>().map(|x| if x > 5.0 { x / 100.0 } else { x }).unwrap_or(1.0)),
            "triple_fov_deg" => v[&k] = json!(val.parse::<f64>().ok().filter(|x| x.is_finite()).map(|x| if x < 20.0 { 0.0 } else { x.min(120.0) }).unwrap_or(0.0)),
            "triple_width_mm" => v[&k] = json!(val.parse::<f64>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(200.0, 2000.0)).unwrap_or(600.0)),
            "triple_distance_mm" => v[&k] = json!(val.parse::<f64>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(200.0, 3000.0)).unwrap_or(650.0)),
            "triple_bezel_mm" => v[&k] = json!(val.parse::<f64>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(0.0, 100.0)).unwrap_or(0.0)),
            "triple_left_angle_deg" => v[&k] = json!(val.parse::<f64>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(0.0, 90.0)).unwrap_or(45.0)),
            "triple_right_angle_deg" => v[&k] = json!(val.parse::<f64>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(0.0, 90.0)).unwrap_or(45.0)),
            "triple_eye_height_mm" => v[&k] = json!(val.parse::<f64>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(-500.0, 500.0)).unwrap_or(0.0)),
            "vr_scale" => v[&k] = json!(val.parse::<f64>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(0.5, 1.0)).unwrap_or(0.65)),
            "vr_head_smoothing_ms" => v[&k] = json!(val.parse::<f64>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(0.0, 30.0) as i64).unwrap_or(0)),
            "vr_mirror_rate" => v[&k] = json!(val.parse::<f64>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(-1.0, 360.0) as i64).unwrap_or(16)),
            "mirror_size" => v[&k] = json!(val.parse::<i64>().map(|x| if x == 0 { 0 } else { x.clamp(64, 2048) }).unwrap_or(256)),
            "mirror_refresh" => v[&k] = json!(mirror_refresh(val)),
            "max_fps" => v[&k] = json!(val.parse::<f64>().map(|x| x as i64).unwrap_or(0)),
            "max_obj_dist" => v[&k] = if val.eq_ignore_ascii_case("auto") { json!("auto") } else { json!(val.parse::<f64>().map(|m| (m.round() as i64).to_string()).unwrap_or_else(|_| "auto".into())) },
            "ssao" | "shadows" | "shadow_blobs" | "navigator" | "enhanced" | "triple_screen" | "triple_span" | "triple_hud_center" | "vr" | "vr_desktop_mirror" | "fullscreen" | "vsync" | "exact_fare" | "detail_textures" | "texture_compression" | "chat" | "tooltips" | "name_tags" | "show_fps" | "clouds" | "doppler" | "driver" | "use_real_time" | "use_real_date" | "use_real_year" | "collision_vehicles" | "collision_objects" | "collision_pedestrians" | "head_movement" | "driverview_smooth" | "hands_in_cab" | "alt_view" => v[&k] = json!(b(val)),
            "maintenance" | "ai_unsched_factor" | "ai_max_scheduled" => v[&k] = json!(val.trim_end_matches('%').parse::<f64>().map(|x| x.max(0.0) as i64).unwrap_or(0)),
            // (-1: no parked cars at all, #864)
            "ai_max_parked" => v[&k] = json!(val.parse::<f64>().map(|x| x.max(-1.0) as i64).unwrap_or(0)),
            "ai_max_humans" => v[&k] = json!(val.parse::<f64>().map(|x| x.max(1.0) as i64).unwrap_or(200)),
            "drive_keys" | "navigator_corner" | "boarding" | "render_scale" | "pax_voices" => v[&k] = json!(val),
            "ctrl_off" => v[&k] = json!(val),
            "metar_station" => v[&k] = json!(val.chars().filter(|c| c.is_ascii_alphabetic()).take(4).collect::<String>().to_ascii_uppercase()),
            "discord_app_id" => v[&k] = json!(val),
            "resolution" | "window_size" => v["resolution"] = json!(resolution_text(val)),
            "graphics_api" => v[&k] = json!(match val.to_ascii_lowercase().as_str() { "vulkan" => "vulkan", "dx12" => "dx12", "gl" => "gl", _ => "auto" }),
            "shadow_casters" => v[&k] = json!(if val.eq_ignore_ascii_case("omsi") { "omsi" } else { "all" }),
            "ctrl_deadzone" => v[&k] = json!(val.parse::<f64>().unwrap_or(0.0).clamp(0.0, 0.3)),
            "mouse_sens" => v[&k] = json!(val.parse::<f64>().unwrap_or(1.0).clamp(0.1, 3.0)),
            "ui_scale" => v[&k] = json!(val.parse::<f64>().ok().filter(|x| x.is_finite()).unwrap_or(1.0).clamp(0.5, 2.0)),
            "wheel_range" => v[&k] = json!(val.parse::<f64>().unwrap_or(900.0).clamp(90.0, 2880.0)),
            "wheel_lock" => v[&k] = json!(val.parse::<f64>().map(|x| if x < 45.0 { 0.0 } else { x.min(2880.0) }).unwrap_or(0.0)),
            "fov" => v[&k] = json!(val.parse::<f64>().map(|x| if x < 20.0 { 0.0 } else { x.min(120.0) }).unwrap_or(0.0)),
            "camera_collision" | "steer_look" | "head_tracking" | "discord_status" | "voice_chat" | "launcher_rest" => v[&k] = json!(b(val)),
            // (how much of the mip chain an LED panel is held at, 0..4; a file from before
            // it was a number says 1 or 0)
            "led_mips" => v[&k] = json!(val.trim().parse::<f64>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(0.0, 4.0)).unwrap_or(1.3)),
            "led_glow" => v[&k] = json!(val.parse::<i64>().map(|x| x.clamp(0, 15)).unwrap_or(6)),
            "look_sens" => v[&k] = json!(val.parse::<f64>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(0.1, 2.0)).unwrap_or(1.0)),
            "steer_look_angle" => v[&k] = json!(val.parse::<f64>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(0.0, 60.0)).unwrap_or(30.0)),
            "steer_look_response" => v[&k] = json!(val.parse::<f64>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(0.05, 1.0)).unwrap_or(0.25)),
            "head_idle" => v[&k] = json!(val.parse::<f64>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(0.0, 1.0)).unwrap_or(0.0)),
            "head_idle_pace" => v[&k] = json!(val.parse::<f64>().ok().filter(|x| x.is_finite()).map(|x| x.clamp(0.5, 2.0)).unwrap_or(1.0)),
            "pedal_throttle" | "pedal_brake" => v[&k] = json!(val.parse::<f64>().map(|x| x.clamp(0.25, 4.0)).unwrap_or(1.0)),
            "seat_x" | "seat_y" | "seat_z" => v[&k] = json!(val.parse::<f64>().map(|x| x.clamp(-1.5, 1.5)).unwrap_or(0.0)),
            "nav_arrows" | "nav_ai" | "get_up" | "time_sync" | "metar_sync" | "ui_scale_window" | "notes" | "machine_translation" | "update_check" | "update_auto" | "reflections" | "steering_linear" | "old_steering" | "red_steer_spd" | "ff_invert" | "ff_enabled" | "brake_hold" | "auto_clutch" | "momentary_gears" | "mouse_steering" | "mouse_right_off" | "blinker_cancel" => v[&k] = json!(b(val)),
            "time_speed" => v[&k] = json!(val.trim_start_matches(['x', 'X']).parse::<f64>().map(|x| x.clamp(1.0, 30.0)).map(|x| if x.fract() == 0.0 { format!("{}", x as i64) } else { x.to_string() }).unwrap_or_else(|_| "1".into())),
            "language" => v[&k] = json!(language_code(val)),
            "graphics" | "renderer" => graphics = Some(graphics_mode(val)),
            // whole metres, as the page's select has them ("1500"); anything else (auto) is
            // the game's default
            "view_distance" => {
                v[&k] = match val.parse::<f64>() {
                    Ok(m) if m > 0.0 => json!((m.round() as i64).to_string()),
                    _ => json!("auto"),
                }
            }
            // MB, 0 = automatic (a fraction written by hand is cut, as the game does)
            "texture_memory" => {
                if let Ok(mb) = val.parse::<f64>() {
                    v[&k] = json!(mb.max(0.0) as i64);
                }
            }
            "version" => version = val.parse::<i64>().unwrap_or(0),
            _ => {}
        }
    }
    // a file without `graphics` (older builds) says only `enhanced`; its vanilla renderer is
    // what is now Vanilla+
    let g = graphics.unwrap_or(if v["enhanced"] == json!(true) { "enhanced" } else { "vanilla_plus" });
    v["graphics"] = json!(g);
    v["enhanced"] = json!(g == "enhanced");
    // before version 2 the launcher wrote its old default `boarding=pay` for everybody
    // (passengers then waited at the cash desk for the driver): the game reads that as auto
    if version < 2 && v["boarding"] == "pay" {
        v["boarding"] = json!("auto");
    }
    v
}

/// OMSI's tutorials (`Tutorials/menu_<n>_<LANG>.html`): per lesson its title and what
/// it teaches, in the settings' language.
pub fn tutorials() -> Vec<(usize, String, String)> {
    let Ok(r) = root() else { return Vec::new() };
    let lang = content_language();
    let mut out = Vec::new();
    for n in 1..=4usize {
        let p = [lang, "ENG", "DEU"].iter().map(|l| r.join("Tutorials").join(format!("menu_{n}_{l}.html"))).find(|p| p.is_file());
        let Some(p) = p else { continue };
        let Ok(bytes) = std::fs::read(&p) else { continue };
        let html = omsi_cfg::codepage::decode(&bytes);
        let body = html.split_once("</style>").map(|x| x.1).unwrap_or(&html);
        let mut text = String::new();
        let mut tag = false;
        for c in body.replace("</p>", "\n").replace("<br>", "\n").replace("</h2>", "\n").replace("<li>", "\n• ").chars() {
            match c {
                '<' => tag = true,
                '>' => tag = false,
                _ if !tag => text.push(c),
                _ => {}
            }
        }
        let text = text.replace("&quot;", "\"").replace("&amp;", "&").replace("&nbsp;", " ");
        let mut lines = text.lines().map(|l| l.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|l| !l.is_empty());
        let title = lines.next().unwrap_or_default();
        let rest: Vec<String> = lines.collect();
        out.push((n, title, rest.join("\n")));
    }
    out
}

/// OMSI's option presets (`option_presets/*.oop`): their names and what they say, in the
/// settings' own keys (maxFPS, performance_minObjSize/maxObjDist, texFilter, texmemlimit,
/// performance_reflTexSize).
pub fn option_presets() -> Vec<(String, Value)> {
    let Ok(r) = root() else { return Vec::new() };
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir(r.join("option_presets")) else { return out };
    let mut files: Vec<PathBuf> = dir.flatten().map(|e| e.path()).filter(|p| p.extension().map(|e| e.eq_ignore_ascii_case("oop")).unwrap_or(false)).collect();
    files.sort();
    for f in files {
        let Ok(o) = omsi_content::options::Options::load(&f) else { continue };
        let name = f.file_stem().unwrap_or_default().to_string_lossy().to_string();
        let mut v = json!({});
        if !cfg!(target_os = "android") {
            v["max_fps"] = json!(o.i32("maxfps", 0).max(0));
        }
        v["min_obj_size"] = json!(o.f32("performance_minobjsize", 0.013) as f64);
        v["max_obj_dist"] = json!((o.f32("performance_maxobjdist", 900.0).round() as i64).to_string());
        if let Some(af) = o.values.get("texfilter").and_then(|x| x.get(1)).and_then(|x| x.parse::<i64>().ok()) {
            v["anisotropy"] = json!(af.clamp(1, 16));
        }
        let mem = o.f32("texmemlimit", 0.0);
        if mem > 0.0 {
            v["texture_memory"] = json!(mem as i64);
        }
        let refl = o.i32("performance_refltexsize", 8);
        v["mirror_size"] = json!(1i64 << refl.clamp(6, 11));
        if let Some(x) = o.str("performance_realreflexions") {
            v["mirror_refresh"] = json!(mirror_refresh(x));
        }
        out.push((name, v));
    }
    out
}

/// The machine's memory in bytes.
#[cfg(unix)]
fn physical_memory() -> Option<u64> {
    // SAFETY: sysconf only reads system values
    let (pages, size) = unsafe { (libc::sysconf(libc::_SC_PHYS_PAGES), libc::sysconf(libc::_SC_PAGESIZE)) };
    (pages > 0 && size > 0).then(|| pages as u64 * size as u64)
}

#[cfg(not(unix))]
fn physical_memory() -> Option<u64> {
    None
}

pub fn save_settings(v: &Value) -> Result<()> {
    let p = data_dir().join("settings.cfg");
    let old = std::fs::read_to_string(&p).ok();
    std::fs::write(p, settings_to_text(v, old.as_deref()))?;
    Ok(())
}

/// The settings a graphics profile holds: what the Graphics tab shows, except the machine's
/// own (fullscreen, graphics API).
pub const GRAPHICS_PROFILE_KEYS: [&str; 21] = [
    "graphics", "msaa", "render_scale", "anisotropy", "shadow_size", "ssao", "shadows", "shadow_casters", "detail_textures", "led_glow", "led_mips", "reflections", "clouds",
    "vsync", "max_fps", "view_distance", "max_obj_dist", "min_obj_size", "mirror_size", "texture_memory", "texture_compression",
];

fn graphics_profiles_path() -> PathBuf {
    data_dir().join("graphics_profiles.json")
}

/// The saved graphics profiles by name (`~/.openomsi/graphics_profiles.json`).
pub fn graphics_profiles() -> std::collections::BTreeMap<String, Value> {
    std::fs::read_to_string(graphics_profiles_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Keep the graphics of `settings` as profile `name` (an existing one of that name is
/// replaced). Returns the name as kept.
pub fn save_graphics_profile(name: &str, settings: &Value) -> Result<String> {
    let name: String = name.chars().filter(|c| !c.is_control()).collect::<String>().trim().chars().take(40).collect();
    if name.is_empty() {
        return Err(anyhow!("Give the profile a name."));
    }
    let mut profile = serde_json::Map::new();
    for k in GRAPHICS_PROFILE_KEYS {
        if let Some(x) = settings.get(k) {
            profile.insert(k.to_string(), x.clone());
        }
    }
    let mut all = graphics_profiles();
    all.insert(name.clone(), Value::Object(profile));
    std::fs::write(graphics_profiles_path(), serde_json::to_string_pretty(&all)?)?;
    Ok(name)
}

/// Remove profile `name`.
pub fn delete_graphics_profile(name: &str) -> Result<()> {
    let mut all = graphics_profiles();
    all.remove(name);
    std::fs::write(graphics_profiles_path(), serde_json::to_string_pretty(&all)?)?;
    Ok(())
}

/// Put a profile's values into the page's `settings` (only the keys a profile may hold).
pub fn apply_graphics_profile(profile: &Value, settings: &mut Value) {
    for k in GRAPHICS_PROFILE_KEYS {
        if let Some(x) = profile.get(k) {
            settings[k] = x.clone();
        }
    }
}

/// The `settings.cfg` text for the page's values `v`; the lines of the `old` file that the
/// page does not manage (keys of newer games, hand-written switches) are kept.
pub fn settings_to_text(v: &Value, old: Option<&str>) -> String {
    let b = |k: &str, d: bool| v.get(k).and_then(|x| x.as_bool()).unwrap_or(d) as u8;
    // a number, also as the string a select gives
    let n = |k: &str, d: i64| v.get(k).and_then(|x| x.as_i64().or_else(|| x.as_f64().or_else(|| x.as_str().and_then(|s| s.trim().parse::<f64>().ok())).map(|f| f as i64))).unwrap_or(d);
    let f = |k: &str, d: f64| v.get(k).and_then(|x| x.as_f64()).unwrap_or(d);
    let text = format!(
        "# openOMSI settings (written by the launcher)\nversion=2\nmsaa={}\nanisotropy={}\nssao={}\nshadows={}\nshadow_size={}\nnavigator={}\nui_opacity={}\nnavigator_corner={}\nboarding={}\ndetail_textures={}\nexact_fare={}\nenhanced={}\ngraphics={}\nfullscreen={}\nvsync={}\nvolume={}\ndrive_keys={}\nrender_scale={}\nview_distance={}\nlanguage={}\ntexture_memory={}\ntexture_compression={}\nchat={}\ntooltips={}\nname_tags={}\nshow_fps={}\nclouds={}\npax_density={}\nvol_ai={}\nvol_scenery={}\nmirror_size={}\ndoppler={}\ndriver={}\nmax_fps={}\nmin_obj_size={}\nmax_obj_dist={}\n",
        n("msaa", 4),
        n("anisotropy", 8),
        b("ssao", true),
        b("shadows", true),
        n("shadow_size", 2048),
        b("navigator", true),
        f("ui_opacity", 0.85),
        v.get("navigator_corner").and_then(|x| x.as_str()).unwrap_or("bottom-left"),
        v.get("boarding").and_then(|x| x.as_str()).unwrap_or("auto"),
        b("detail_textures", true),
        b("exact_fare", true),
        (graphics_mode(v.get("graphics").and_then(|x| x.as_str()).unwrap_or("vanilla_plus")) == "enhanced") as u8,
        graphics_mode(v.get("graphics").and_then(|x| x.as_str()).unwrap_or("vanilla_plus")),
        b("fullscreen", false),
        b("vsync", true),
        f("volume", 0.6),
        v.get("drive_keys").and_then(|x| x.as_str()).unwrap_or("simple"),
        // "auto" or a fraction, as a string (the page's select) or a number
        match v.get("render_scale") {
            Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
            Some(Value::Number(x)) => x.to_string(),
            _ => "auto".to_string(),
        },
        // metres, or "auto" (the game's 1200 m)
        match v.get("view_distance") {
            Some(Value::String(s)) if s.trim().parse::<f64>().map(|m| m > 0.0).unwrap_or(false) => s.trim().to_string(),
            Some(Value::Number(x)) if x.as_f64().map(|m| m > 0.0).unwrap_or(false) => x.to_string(),
            _ => "auto".to_string(),
        },
        language_code(v.get("language").and_then(|x| x.as_str()).unwrap_or("ENG")),
        n("texture_memory", 0).max(0),
        b("texture_compression", true),
        b("chat", true),
        b("tooltips", true),
        b("name_tags", true),
        b("show_fps", false),
        b("clouds", true),
        f("pax_density", 1.0),
        f("vol_ai", 1.0),
        f("vol_scenery", 1.0),
        match n("mirror_size", 256) { 0 => 0, x => x.clamp(64, 2048) },
        b("doppler", true),
        b("driver", true),
        n("max_fps", 0).max(0),
        f("min_obj_size", 0.013),
        match v.get("max_obj_dist") {
            Some(Value::String(s)) if s.trim().parse::<f64>().is_ok() => s.trim().to_string(),
            Some(Value::Number(x)) => x.to_string(),
            _ => "auto".to_string(),
        },
    );
    // OMSI's own options (options.cfg): maintenance ([wear_lifespan]), the AI counts and
    // the share of random traffic, the real clock and calendar, collisions, head movement
    let text = format!(
        "{text}maintenance={}\nai_unsched_factor={}\nai_max_scheduled={}\nai_max_parked={}\nai_max_humans={}\nuse_real_time={}\nuse_real_date={}\nuse_real_year={}\ncollision_vehicles={}\ncollision_objects={}\ncollision_pedestrians={}\nhead_movement={}\ndriverview_smooth={}\nhands_in_cab={}\nalt_view={}\n",
        n("maintenance", 0).clamp(0, 4),
        n("ai_unsched_factor", 100).clamp(0, 300),
        n("ai_max_scheduled", 0).max(0),
        n("ai_max_parked", 0).max(-1),
        n("ai_max_humans", 200).max(1),
        b("use_real_time", false),
        b("use_real_date", false),
        b("use_real_year", false),
        b("collision_vehicles", true),
        b("collision_objects", true),
        b("collision_pedestrians", true),
        b("head_movement", true),
        b("driverview_smooth", true),
        b("hands_in_cab", false),
        b("alt_view", true),
    );
    let text = format!(
        "{text}pax_voices={}\nnav_arrows={}\nnav_ai={}\nget_up={}\ntime_speed={}\nmachine_translation={}\nshadow_casters={}\nshadow_blobs={}\nctrl_deadzone={}\nupdate_check={}\nupdate_auto={}\nreflections={}\nmouse_sens={}\ngraphics_api={}\nctrl_off={}\nsteering_linear={}\nold_steering={}\nred_steer_spd={}\nff_invert={}\nwheel_range={}\nwheel_lock={}\nfov={}\ncamera_collision={}\npedal_throttle={}\npedal_brake={}\nseat_x={}\nseat_y={}\nseat_z={}\nsteer_look={}\nhead_tracking={}\nff_enabled={}\nbrake_hold={}\nauto_clutch={}\nmomentary_gears={}\nled_glow={}\nled_mips={}\nui_scale={}\nui_scale_window={}\nnotes={}\nmouse_steering={}\nmouse_right_off={}\nblinker_cancel={}\n",
        match v.get("pax_voices").and_then(|x| x.as_str()).unwrap_or("all") {
            "tickets" => "tickets",
            "off" => "off",
            _ => "all",
        },
        b("nav_arrows", false),
        b("nav_ai", true),
        b("get_up", false),
        match v.get("time_speed") {
            Some(Value::String(x)) => x.trim().parse::<f64>().map(|x| x.clamp(1.0, 30.0)).unwrap_or(1.0),
            Some(Value::Number(x)) => x.as_f64().unwrap_or(1.0).clamp(1.0, 30.0),
            _ => 1.0,
        },
        b("machine_translation", false),
        if v.get("shadow_casters").and_then(|x| x.as_str()) == Some("omsi") { "omsi" } else { "all" },
        b("shadow_blobs", true),
        f("ctrl_deadzone", 0.0).clamp(0.0, 0.3),
        b("update_check", true),
        b("update_auto", false),
        b("reflections", true),
        f("mouse_sens", 1.0).clamp(0.1, 3.0),
        match v.get("graphics_api").and_then(|x| x.as_str()).unwrap_or("auto") {
            "vulkan" => "vulkan",
            "dx12" => "dx12",
            "gl" => "gl",
            _ => "auto",
        },
        v.get("ctrl_off").and_then(|x| x.as_str()).unwrap_or("").replace(['\n', '\r'], " "),
        b("steering_linear", false),
        b("old_steering", false),
        b("red_steer_spd", false),
        b("ff_invert", false),
        f("wheel_range", 900.0).clamp(90.0, 2880.0),
        f("wheel_lock", 0.0).clamp(0.0, 2880.0),
        f("fov", 0.0).clamp(0.0, 120.0),
        b("camera_collision", true),
        f("pedal_throttle", 1.0).clamp(0.25, 4.0),
        f("pedal_brake", 1.0).clamp(0.25, 4.0),
        f("seat_x", 0.0).clamp(-1.5, 1.5),
        f("seat_y", 0.0).clamp(-1.5, 1.5),
        f("seat_z", 0.0).clamp(-1.5, 1.5),
        b("steer_look", false),
        b("head_tracking", false),
        b("ff_enabled", true),
        b("brake_hold", true),
        b("auto_clutch", true),
        b("momentary_gears", false),
        n("led_glow", 6).clamp(0, 15),
        f("led_mips", 1.3).clamp(0.0, 4.0),
        f("ui_scale", 1.0).clamp(0.5, 2.0),
        b("ui_scale_window", true),
        b("notes", true),
        b("mouse_steering", false),
        b("mouse_right_off", false),
        b("blinker_cancel", true),
    );
    let vr_scale = v.get("vr_scale").and_then(|x| x.as_f64().or_else(|| x.as_str().and_then(|s| s.parse().ok()))).filter(|x| x.is_finite()).unwrap_or(0.65).clamp(0.5, 1.0);
    let vr_head_smoothing_ms = v.get("vr_head_smoothing_ms").and_then(|x| x.as_f64().or_else(|| x.as_str().and_then(|s| s.parse().ok()))).filter(|x| x.is_finite()).unwrap_or(0.0).clamp(0.0, 30.0);
    let vr_mirror_rate = v.get("vr_mirror_rate").and_then(|x| x.as_f64().or_else(|| x.as_str().and_then(|s| s.parse().ok()))).filter(|x| x.is_finite()).unwrap_or(16.0).clamp(-1.0, 360.0);
    let text = format!("{text}vr={}\nvr_scale={vr_scale}\nvr_head_smoothing_ms={vr_head_smoothing_ms}\nvr_mirror_rate={vr_mirror_rate}\nvr_desktop_mirror={}\ndiscord_status={}\nvoice_chat={}\nlauncher_rest={}\n", b("vr", false), b("vr_desktop_mirror", true), b("discord_status", true), b("voice_chat", true), b("launcher_rest", true));
    // what the page does not manage (keys of newer games, hand-written ones) stays as it
    // was in the file; other spellings of the keys just written go
    let mut text = text;
    text.push_str(&format!("resolution={}\n", resolution_text(v.get("resolution").and_then(|x| x.as_str()).unwrap_or("auto"))));
    text.push_str(&format!("mirror_refresh={}\n", mirror_refresh(v.get("mirror_refresh").and_then(|x| x.as_str()).unwrap_or("full"))));
    text.push_str(&format!("look_sens={}\nsteer_look_angle={}\nsteer_look_response={}\nhead_idle={}\nhead_idle_pace={}\ntime_sync={}\nmetar_sync={}\nmetar_station={}\n", f("look_sens", 1.0).clamp(0.1, 2.0), f("steer_look_angle", 30.0).clamp(0.0, 60.0), f("steer_look_response", 0.25).clamp(0.05, 1.0), f("head_idle", 0.0).clamp(0.0, 1.0), f("head_idle_pace", 1.0).clamp(0.5, 2.0), b("time_sync", false), b("metar_sync", false), v.get("metar_station").and_then(|x| x.as_str()).unwrap_or("").chars().filter(|c| c.is_ascii_alphabetic()).take(4).collect::<String>().to_ascii_uppercase()));
    let triple_fov = f("triple_fov_deg", 0.0);
    text.push_str(&format!("triple_hud_center={}\ntriple_fov_deg={}\n", b("triple_hud_center", true), if triple_fov < 20.0 { 0.0 } else { triple_fov.min(120.0) }));
    text.push_str(&format!("triple_screen={}\ntriple_span={}\n", b("triple_screen", false), b("triple_span", true)));
    text.push_str(&format!("triple_width_mm={}\ntriple_distance_mm={}\ntriple_bezel_mm={}\n", f("triple_width_mm", 600.0).clamp(200.0, 2000.0), f("triple_distance_mm", 650.0).clamp(200.0, 3000.0), f("triple_bezel_mm", 0.0).clamp(0.0, 100.0)));
    text.push_str(&format!("triple_left_angle_deg={}\ntriple_right_angle_deg={}\ntriple_eye_height_mm={}\n", f("triple_left_angle_deg", 45.0).clamp(0.0, 90.0), f("triple_right_angle_deg", 45.0).clamp(0.0, 90.0), f("triple_eye_height_mm", 0.0).clamp(-500.0, 500.0)));
    let written: Vec<String> = text.lines().filter_map(|l| l.split_once('=')).map(|(k, _)| k.trim().to_ascii_lowercase()).collect();
    for line in old.unwrap_or("").lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') || t.starts_with(';') {
            continue;
        }
        match t.split_once('=') {
            Some((k, _)) if !written.contains(&setting_key(k)) => {
                text.push_str(t);
                text.push('\n');
            }
            _ => {}
        }
    }
    text
}

// ---------------------------------------------------------------------------------------
// the 3D preview and launching the game

/// Ask the game to write the bus as glTF (cached by bus and paint) and return the path.
pub fn bus_preview(bus: &str, paint: &str) -> Result<String> {
    let c = load_config();
    let game = find_game(&c.game).context("the game binary was not found (set it under Setup)")?;
    let root = root()?;
    let key: String = format!("{bus}|{paint}").chars().map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' }).collect();
    let out = data_dir().join("cache").join(format!("{key}.glb"));
    let bus_path = resolve_content(bus)?;
    // a bus inside an archive used in place changes with the archive
    let bus_path = omsi_cfg::vfs::archive_of(&bus_path).unwrap_or(bus_path);
    let newest_input = [&bus_path, &game].iter().filter_map(|p| p.metadata().and_then(|m| m.modified()).ok()).max();
    let fresh = out.metadata().and_then(|m| m.modified()).ok().zip(newest_input).map(|(o, b)| o >= b).unwrap_or(false);
    if !fresh {
        let mut cmd = std::process::Command::new(&game);
        cmd.arg("--root").arg(&root).arg("--bus").arg(bus).arg("--export-glb").arg(&out);
        if !paint.trim().is_empty() {
            cmd.arg("--paint").arg(paint.trim());
        }
        let status = cmd.env("RUST_LOG", "warn").status().context("running the game for the preview")?;
        if !status.success() || !out.exists() {
            return Err(anyhow!("the game could not export {bus}"));
        }
    }
    Ok(out.to_string_lossy().to_string())
}

#[derive(Deserialize, Default, Debug, Clone)]
pub struct Duty {
    pub map: String,
    pub bus: String,
    pub paint: Option<String>,
    /// The number plate (registration) the player typed: it wins over the plate the bus's
    /// `[number]` list or the map's `registrations.txt` gives it (empty: as the content says).
    #[serde(default)]
    pub plate: Option<String>,
    /// The fleet number picked from the bus's `[number]` list (none: its first).
    #[serde(default)]
    pub number: Option<String>,
    pub hof: Option<String>,
    pub entry: Option<i32>,
    pub line: Option<String>,
    pub tour: Option<String>,
    /// The trip of the tour to start with: its departure (HH:MM) or its place in the tour.
    #[serde(default)]
    pub trip: Option<String>,
    #[serde(default)]
    pub whole_tour: bool,
    /// HH:MM
    pub time: String,
    /// YYYY-MM-DD
    pub date: Option<String>,
    pub weather: Option<String>,
    pub traffic: Option<u32>,
    pub passengers: Option<bool>,
    pub schedule: Option<bool>,
    pub autostart: Option<bool>,
    /// Start as a pedestrian beside the bus (which is then left out): a bus is placed or
    /// taken over from the game menu later.
    #[serde(default)]
    pub on_foot: Option<bool>,
    pub profile: Option<String>,
    /// LAN play: "host", or "join:<session code | ip[:port] | port | empty = search>".
    pub lan: Option<String>,
    /// The name the other LAN players see (default: the profile).
    pub lan_name: Option<String>,
    /// Season override: spring / summer / autumn / winter (empty = by date).
    pub season: Option<String>,
    /// One of OMSI's tutorials (1..4): its own situation, nothing else of the duty.
    #[serde(default)]
    pub tutorial: Option<usize>,
    /// A situation file to continue (the map's `laststn.osn`): nothing else of the duty.
    #[serde(default)]
    pub situation: Option<String>,
}

/// The situation the game left on `map` last (`laststn.osn` in the map's folder: the
/// content folder's copy first, then OMSI 2's own), if there is one.
pub fn last_situation(map: &str) -> Option<PathBuf> {
    let dir = Path::new(&map.replace('\\', "/")).parent()?.to_path_buf();
    content_dir()
        .map(|c| c.join(&dir).join("laststn.osn"))
        .into_iter()
        .chain(root().ok().map(|r| r.join(&dir).join("laststn.osn")))
        .find(|p| p.is_file())
}

/// A situation saved on a map to continue from: the file, its `[name]`, when it was written
/// (seconds since 1970).
#[derive(Debug, Clone, PartialEq)]
pub struct SavedSituation {
    pub file: PathBuf,
    pub name: String,
    pub saved: u64,
}

/// What can be continued on `map` (#341): the last situation, then the save slots the game
/// writes into `Saves` of the map's folder in the content folder, the newest first.
pub fn saved_situations(map: &str) -> Vec<SavedSituation> {
    let mut out: Vec<SavedSituation> = last_situation(map).map(|f| SavedSituation { saved: modified_secs(&f), file: f, name: "Last situation".into() }).into_iter().collect();
    if let (Some(dir), Some(c)) = (Path::new(&map.replace('\\', "/")).parent(), content_dir()) {
        out.extend(save_slots(&c.join(dir).join("Saves")));
    }
    out
}

fn modified_secs(p: &Path) -> u64 {
    std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0)
}

/// The situations in a map's `Saves` folder, the newest first, by their `[name]`.
fn save_slots(dir: &Path) -> Vec<SavedSituation> {
    let mut slots: Vec<SavedSituation> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("osn")))
        .map(|f| {
            // (the `[name]` line alone: the lists ask again every few seconds, and a whole
            // situation holds every variable of every vehicle; the game writes them in
            // UTF-16, as OMSI does)
            let text = std::fs::read(&f).map(|b| omsi_cfg::decode_text(&b[..b.len().min(8192) & !1])).unwrap_or_default();
            let mut lines = text.lines().map(str::trim);
            let name = lines.by_ref().find(|l| l.eq_ignore_ascii_case("[name]")).and_then(|_| lines.next()).map(str::to_string).filter(|n| !n.is_empty()).unwrap_or_else(|| f.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default());
            SavedSituation { saved: modified_secs(&f), file: f, name }
        })
        .collect();
    slots.sort_by(|a, b| b.saved.cmp(&a.saved).then_with(|| b.name.cmp(&a.name)));
    slots
}

#[cfg(test)]
mod save_slot_tests {
    use super::*;

    #[test]
    fn the_slots_of_a_map_are_listed_by_their_names() {
        let dir = std::env::temp_dir().join(format!("omsi_slots_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // as the game writes them: UTF-16 with its mark
        let utf16 = |t: &str| [0xFFu8, 0xFE].into_iter().chain(t.encode_utf16().flat_map(|u| u.to_le_bytes())).collect::<Vec<u8>>();
        std::fs::write(dir.join("Slot 1.osn"), utf16("\r\n[name]\r\nSlot 1: SD202, 09:00\r\n[description]\r\nx\r\n")).unwrap();
        std::fs::write(dir.join("Slot 2.osn"), utf16("[name]\r\nSlot 2: NG272, 10:30\r\n")).unwrap();
        std::fs::write(dir.join("notes.txt"), "not a situation").unwrap();
        let mut names: Vec<String> = save_slots(&dir).into_iter().map(|s| s.name).collect();
        names.sort();
        assert_eq!(names, ["Slot 1: SD202, 09:00", "Slot 2: NG272, 10:30"]);
        assert!(save_slots(&dir.join("none")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// The command line a duty becomes.
pub fn duty_args(d: &Duty) -> Result<Vec<String>> {
    let root = root()?;
    if let Some(t) = d.tutorial {
        return Ok(vec!["--root".into(), root.to_string_lossy().to_string(), "--no-menu".into(), "--tutorial".into(), t.to_string()]);
    }
    if let Some(sit) = d.situation.as_deref().filter(|s| !s.trim().is_empty()) {
        let mut a = vec!["--root".into(), root.to_string_lossy().to_string(), "--no-menu".into(), "--situation".into(), sit.to_string()];
        if let Some(p) = d.profile.as_deref().filter(|p| !p.trim().is_empty()) {
            a.extend(["--driver".into(), format!("Drivers/{}.odr", p.trim())]);
        }
        // the traffic and the people as for any other start: a situation file keeps the
        // vehicles the player placed, not how busy the streets are (OMSI takes that from its
        // options), and without these a continued session had the timetable buses alone -
        // no cars, nobody at the stops (#136)
        a.extend(["--traffic".into(), d.traffic.unwrap_or(30).to_string()]);
        if d.passengers.unwrap_or(true) {
            a.push("--passengers".into());
        }
        return Ok(a);
    }
    let mut a: Vec<String> = vec!["--root".into(), root.to_string_lossy().to_string(), "--no-menu".into(), "--map".into(), d.map.clone(), "--bus".into(), d.bus.clone(), "--time".into(), if d.time.trim().is_empty() { "09:00".into() } else { d.time.trim().to_string() }];
    if let Some(p) = d.paint.as_deref().filter(|p| !p.trim().is_empty()) {
        a.extend(["--paint".into(), p.trim().to_string()]);
    }
    if let Some(pl) = d.plate.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        a.extend(["--plate".into(), pl.to_string()]);
    }
    if let Some(n) = d.number.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        a.extend(["--number".into(), n.to_string()]);
    }
    // (a vehicle file taken for a depot from a broken ailists.cfg by older launchers is none)
    if let Some(h) = d.hof.as_deref().filter(|h| !h.trim().is_empty() && !h.to_ascii_lowercase().contains(".bus") && !h.to_ascii_lowercase().contains(".ovh")) {
        a.extend(["--hof".into(), h.trim().to_string()]);
    }
    // the entry point's place in the map's list; -1: the one nearest to the duty's first
    // stop (a free drive takes the list's first then)
    match d.entry {
        Some(e) if e < 0 => {
            if d.line.as_deref().map(|l| !l.trim().is_empty()).unwrap_or(false) {
                a.push("--auto-entry".into());
            }
        }
        Some(e) => a.extend(["--entry".into(), e.to_string()]),
        None => {}
    }
    // OMSI's [useActTime] / [useActDate] / [useActYear]: the machine's clock and calendar
    // instead of the duty's (the year only on its own switch - a map's timetable is for
    // its years)
    let st = get_settings().unwrap_or_default();
    let on = |k: &str| st.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
    let now = local_now();
    if let (true, Some((_, _, _, h, m))) = (on("use_real_time"), now) {
        if let Some(i) = a.iter().position(|x| x == "--time") {
            a[i + 1] = format!("{h:02}:{m:02}");
        }
    }
    let mut date = d.date.as_deref().map(|x| x.trim().to_string()).filter(|x| !x.is_empty());
    if let (true, Some((y, mo, dd, _, _))) = (on("use_real_date"), now) {
        let year = if on("use_real_year") { y } else { date.as_deref().and_then(|x| x.split('-').next()?.parse::<i32>().ok()).unwrap_or(y) };
        date = Some(format!("{year:04}-{mo:02}-{dd:02}"));
    }
    if let Some(dt) = date {
        a.extend(["--date".into(), dt]);
    }
    if let Some(w) = d.weather.as_deref().filter(|x| !x.trim().is_empty()) {
        a.extend(["--weather".into(), w.trim().to_string()]);
    }
    a.extend(["--traffic".into(), d.traffic.unwrap_or(30).to_string()]);
    if d.passengers.unwrap_or(true) {
        a.push("--passengers".into());
    }
    let schedule = d.schedule.unwrap_or(true) || d.line.is_some();
    if schedule {
        a.push("--schedule".into());
    }
    if let (Some(l), true) = (d.line.as_deref().filter(|x| !x.trim().is_empty()), schedule) {
        a.extend(["--line".into(), l.trim().to_string()]);
        if let Some(t) = d.tour.as_deref().filter(|x| !x.trim().is_empty()) {
            a.extend(["--tour".into(), t.trim().to_string()]);
            if let Some(tr) = d.trip.as_deref().filter(|x| !x.trim().is_empty()) {
                a.extend(["--trip".into(), tr.trim().to_string()]);
                if d.whole_tour {
                    a.push("--whole-tour".into());
                }
            }
        }
    }
    if d.autostart.unwrap_or(false) {
        a.push("--autostart".into());
    }
    if d.on_foot.unwrap_or(false) {
        a.push("--on-foot".into());
    }
    let profile = d.profile.clone().filter(|p| !p.trim().is_empty()).unwrap_or_else(|| load_config().profile);
    if let Some(season) = d.season.as_deref().map(str::trim).filter(|x| !x.is_empty() && !x.eq_ignore_ascii_case("auto")) {
        a.extend(["--season".into(), season.to_ascii_lowercase()]);
    }
    if let Some(lan) = d.lan.as_deref().map(str::trim).filter(|l| !l.is_empty() && !l.eq_ignore_ascii_case("off")) {
        if lan.eq_ignore_ascii_case("host") {
            // 0: the default port, or the next free one when a session runs here already
            a.extend(["--lan-host".into(), "0".into()]);
        } else if let Some(target) = lan.strip_prefix("join:") {
            let target = target.trim();
            omsi_net::describe_join(target).map_err(|e| anyhow!("LAN join: {e}"))?;
            a.extend(["--lan-join".into(), if target.is_empty() { "auto".into() } else { target.to_string() }]);
        } else {
            return Err(anyhow!("LAN play: '{lan}' is neither host nor join:<code or address>"));
        }
        let name = d.lan_name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| profile.clone());
        if !name.trim().is_empty() {
            a.extend(["--lan-name".into(), name.trim().to_string()]);
        }
    }
    if !profile.trim().is_empty() {
        a.extend(["--driver".into(), format!("Drivers/{}.odr", profile.trim())]);
    }
    Ok(a)
}

#[derive(Serialize, Clone, Debug)]
pub struct Launched {
    pub pid: u32,
    pub log: String,
    pub command: String,
    /// Games that were running already (and keep running).
    pub others: usize,
}

/// Start a game for the duty. Any number may run at once; each writes its own log.
pub fn launch(d: &Duty) -> Result<Launched> {
    if IN_PROCESS_GAMES {
        let args = duty_args(d)?;
        let command = args.join(" ");
        log_to_file(&format!("game in this process: {command}"));
        *IN_PROCESS.lock().unwrap_or_else(|e| e.into_inner()) = Some(args);
        return Ok(Launched { pid: std::process::id(), log: data_dir().join("game.log").to_string_lossy().to_string(), command, others: 0 });
    }
    let c = load_config();
    let game = find_game(&c.game).context("the game binary was not found (set it under Setup)")?;
    let args = duty_args(d)?;
    let profile = d.profile.clone().filter(|p| !p.trim().is_empty()).unwrap_or(c.profile);
    let s = instances::start(&game, &args, d, &profile)?;
    Ok(Launched { pid: s.pid, log: s.log.to_string_lossy().to_string(), command: s.command, others: s.others })
}

/// What a LAN join field means (or what is wrong with it), and the sessions hosted here.
pub fn check_join(text: &str) -> Value {
    match omsi_net::describe_join(text) {
        Ok(d) => json!({ "ok": true, "text": d, "local": instances::local_hosts() }),
        Err(e) => json!({ "ok": false, "text": e, "local": instances::local_hosts() }),
    }
}

// ---------------------------------------------------------------------------------------
// small services for the window

/// A line into ~/.openomsi/launcher.log.
pub fn log_to_file(line: &str) {
    use std::io::Write;
    let p = data_dir().join("launcher.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = writeln!(f, "{now} {line}");
    }
}

/// What interrupted installs left behind (and the old unzip folder): cleaned up once when
/// the launcher opens.
pub fn cleanup() {
    for line in install::cleanup_stale(&data_dir(), content_dir().as_deref()) {
        log_to_file(&format!("cleanup: {line}"));
    }
}

/// Native folder / file picker (Finder, Explorer, the GTK dialog) for a mod. Must run on
/// the main thread. (None on a phone: the launcher browses the storage itself there.)
pub fn pick_mod(zip: bool) -> Option<PathBuf> {
    #[cfg(not(target_os = "android"))]
    {
        if zip {
            rfd::FileDialog::new().set_title("Choose a mod archive").add_filter("Mod archive", &["zip", "7z", "rar"]).pick_file()
        } else {
            rfd::FileDialog::new().set_title("Choose the mod folder").pick_folder()
        }
    }
    #[cfg(target_os = "android")]
    {
        let _ = zip;
        None
    }
}

/// Folder picker (Setup: the OMSI 2 folder).
pub fn pick_folder(title: &str) -> Option<PathBuf> {
    #[cfg(not(target_os = "android"))]
    {
        rfd::FileDialog::new().set_title(title).pick_folder()
    }
    #[cfg(target_os = "android")]
    {
        let _ = title;
        None
    }
}

/// File picker (Setup: the game program).
pub fn pick_file(title: &str) -> Option<PathBuf> {
    #[cfg(not(target_os = "android"))]
    {
        rfd::FileDialog::new().set_title(title).pick_file()
    }
    #[cfg(target_os = "android")]
    {
        let _ = title;
        None
    }
}

/// A phone runs one program: the launcher hands the game's command line over here and the
/// same process plays it in the same window (see the app's `android.rs`) instead of starting
/// another process.
static IN_PROCESS: std::sync::Mutex<Option<Vec<String>>> = std::sync::Mutex::new(None);

/// The command line of a game the launcher asked for (taken once).
pub fn take_in_process_launch() -> Option<Vec<String>> {
    IN_PROCESS.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// Whether games run inside the launcher's own process (a phone).
pub const IN_PROCESS_GAMES: bool = cfg!(target_os = "android");

pub use instances::{list as list_instances, log_tail, stop as stop_instance, Instance};

/// Terminal access to the same functions: `--cli lines '{"map":"maps/Grundorf/global.cfg"}'`.
pub fn cli(cmd: &str, arg: &str) -> Result<Value> {
    let a: Value = serde_json::from_str(arg).unwrap_or(json!({}));
    let s = |k: &str| a.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    Ok(match cmd {
        "config" => serde_json::to_value(load_config())?,
        "maps" => serde_json::to_value(list_maps()?)?,
        "vehicles" => serde_json::to_value(list_vehicles()?)?,
        "weather" => serde_json::to_value(list_weather()?)?,
        "lines" => serde_json::to_value(list_lines(&s("map"), &s("date"))?)?,
        "ibis" => serde_json::to_value(ibis_info(&s("bus"), &s("hof"), &s("line"))?)?,
        "profiles" => serde_json::to_value(list_profiles()?)?,
        "profile" => serde_json::to_value(get_profile(&s("name"))?)?,
        "mods" => {
            // the inbox is installed right away here (there is no page to follow it)
            let done = install_inbox_blocking();
            let mut v = serde_json::to_value(mods_status()?)?;
            v["installed_now"] = serde_json::to_value(done)?;
            v
        }
        "install" => {
            let cancel = a.get("cancel_after_ms").and_then(|v| v.as_u64());
            let mode = s("mode");
            let p = install_mod_blocking(Path::new(&s("path")), if mode.is_empty() { "auto" } else { mode.as_str() }, cancel)?;
            if p.state == "failed" {
                return Err(anyhow!("{}", p.message));
            }
            serde_json::to_value(p)?
        }
        "modinfo" => serde_json::to_value(inspect_mod(Path::new(&s("path")))?)?,
        "stamp" => json!(poll()?.stamp),
        "poll" => {
            // {"watch": seconds}: keep polling like the page does (the inbox watcher needs
            // two looks), then wait for the installs it started
            let watch = a.get("watch").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let t0 = std::time::Instant::now();
            let mut stamps = vec![poll()?.stamp];
            let mut started = Vec::new();
            while t0.elapsed().as_secs_f64() < watch || install::jobs().iter().any(|j| j.finished.is_none()) {
                std::thread::sleep(std::time::Duration::from_millis(1000));
                let p = poll()?;
                started.extend(p.started);
                if stamps.last() != Some(&p.stamp) {
                    stamps.push(p.stamp);
                }
            }
            let mut v = serde_json::to_value(poll()?)?;
            v["started"] = json!(started);
            v["stamps"] = json!(stamps);
            v
        }
        "instances" => serde_json::to_value(instances::list())?,
        "stop" => {
            let by_itself = instances::stop(a.get("pid").and_then(|v| v.as_u64()).unwrap_or(0) as u32)?;
            json!({ "stopped": true, "ended_by_itself": by_itself })
        }
        "log" => json!(instances::log_tail(a.get("pid").and_then(|v| v.as_u64()).unwrap_or(0) as u32, a.get("lines").and_then(|v| v.as_u64()).unwrap_or(40) as usize)?),
        "join" => check_join(&s("text")),
        "settings" => get_settings()?,
        // what the page's Save does: `--cli save_settings '{"view_distance":"1500"}'` (the
        // other values from the file as it is)
        "save_settings" => {
            let mut v = get_settings()?;
            if let (Some(v), Some(changes)) = (v.as_object_mut(), a.as_object()) {
                v.extend(changes.clone());
            }
            save_settings(&v)?;
            get_settings()?
        }
        "keybindings" => get_keybindings()?,
        // `--cli save_keybindings '{"game":[...],"vehicles":[...]}'`: the whole list, as
        // `keybindings` returns it - a partial update reads the current file first
        "save_keybindings" => {
            save_keybindings(&a)?;
            get_keybindings()?
        }
        "preview" => json!(bus_preview(&s("bus"), &s("paint"))?),
        "args" => json!(duty_args(&serde_json::from_value(a.clone())?)?),
        "launch" => serde_json::to_value(launch(&serde_json::from_value(a.clone())?)?)?,
        _ => return Err(anyhow!("unknown command {cmd}")),
    })
}


#[cfg(test)]
mod tests {
    #[test]
    fn vehicle_type_label_falls_back_to_the_file_name() {
        let path = std::path::Path::new("Vehicles/Pack/NL_202.bus");
        for empty in ["", "   "] {
            assert_eq!(super::vehicle_type_label(empty, path), "NL 202");
        }
        assert_eq!(super::vehicle_type_label("  MAN_NL202  ", path), "MAN NL202");
    }

    #[test]
    fn a_part_found_from_the_vehicle_folder_is_no_missing_pack() {
        let root = std::env::temp_dir().join(format!("openomsi-packs-{}", std::process::id()));
        let obj = root.join("Sceneryobjects/X");
        let cfgs = root.join("Vehicles/B/model/Configuration Files");
        std::fs::create_dir_all(&obj).unwrap();
        std::fs::create_dir_all(&cfgs).unwrap();
        std::fs::write(obj.join("a.o3d"), b"x").unwrap();
        omsi_cfg::add_content_root(root.clone());
        let model = cfgs.join("m.cfg");
        std::fs::write(&model, "[mesh]\r\n..\\..\\..\\Sceneryobjects\\X\\a.o3d\r\n..\\..\\..\\Other\\b.o3d\r\n").unwrap();
        let packs = super::missing_packs_of(&model);
        omsi_cfg::remove_content_root(&root);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(packs, vec!["Other".to_string()]);
    }

    #[test]
    fn the_games_options_survive_a_save() {
        // what the pause menu's Options change, read back as they were set
        let mut v = settings_from_text(None);
        for (k, x) in [("steer_look", json!(true)), ("discord_status", json!(false)), ("voice_chat", json!(false)), ("launcher_rest", json!(false)), ("camera_collision", json!(false)), ("brake_hold", json!(false)), ("auto_clutch", json!(false)), ("momentary_gears", json!(true)), ("ff_enabled", json!(false)), ("head_tracking", json!(true)), ("collision_objects", json!(false)), ("led_mips", json!(2.5)), ("led_glow", json!(11)), ("look_sens", json!(0.5)), ("blinker_cancel", json!(false)), ("pedal_brake", json!(1.5)), ("head_idle", json!(0.35)), ("head_idle_pace", json!(1.5)), ("seat_y", json!(-0.1))] {
            v[k] = x;
        }
        let back = settings_from_text(Some(&settings_to_text(&v, None)));
        let mut r = settings_from_text(None);
        assert_eq!(r["resolution"], json!("auto"));
        r["resolution"] = json!("1280x800");
        assert_eq!(settings_from_text(Some(&settings_to_text(&r, None)))["resolution"], json!("1280x800"));
        assert_eq!(resolution_text("1920 x 1080"), "1920x1080");
        assert_eq!(resolution_text("huge"), "auto");
        for k in ["steer_look", "discord_status", "voice_chat", "launcher_rest", "camera_collision", "brake_hold", "auto_clutch", "momentary_gears", "ff_enabled", "head_tracking", "collision_objects", "led_mips", "led_glow", "look_sens", "blinker_cancel", "pedal_brake", "head_idle", "head_idle_pace", "seat_y"] {
            assert_eq!(back[k], v[k], "{k}");
        }
        assert!(settings_from_text(None)["discord_status"].as_bool().unwrap());
        assert!(settings_from_text(None)["launcher_rest"].as_bool().unwrap());
        assert!(settings_from_text(None)["voice_chat"].as_bool().unwrap());
        let prior = settings_from_text(Some("discord_status=1\ndiscord_status=0\n"));
        assert!(!prior["discord_status"].as_bool().unwrap());
        let mut enabled = prior;
        enabled["discord_status"] = json!(true);
        let saved = settings_to_text(&enabled, Some("discord_status=0\ndiscord_status=0\n"));
        assert_eq!(saved.lines().filter(|line| line.starts_with("discord_status=")).count(), 1);
        assert!(settings_from_text(Some(&saved))["discord_status"].as_bool().unwrap());
        let custom_id = "discord_app_id=123456\n";
        let values = settings_from_text(Some(custom_id));
        assert_eq!(values["discord_app_id"], json!("123456"));
        let saved = settings_to_text(&values, Some(custom_id));
        assert_eq!(settings_from_text(Some(&saved))["discord_app_id"], json!("123456"));
    }

    #[test]
    fn dsc_files_give_name_and_description() {
        let d = super::parse_dsc("\r\n[friendlyname]\r\nMAN\r\nNL202 - EN92\r\nBeige\r\n\r\n[description]\r\nAlthough the BVG did not purchase\r\n\r\n-Technical specifications-\r\n[end]\r\n");
        assert_eq!(d.name, vec!["MAN", "NL202 - EN92", "Beige"]);
        assert_eq!(d.description, "Although the BVG did not purchase\n\n-Technical specifications-");
        let w = super::parse_dsc("[name]\r\nGround Fog\r\n\r\n[description]\r\nHeavy ground fog limits the maximum visibility dangerously!\r\n[end]\r\n");
        assert_eq!(w.name, vec!["Ground Fog"]);
        assert_eq!(w.description, "Heavy ground fog limits the maximum visibility dangerously!");
        let p = std::path::Path::new("/x/maps/Spandau/global.cfg");
        assert_eq!(super::dsc_candidates(p, "ENG"), vec![std::path::PathBuf::from("/x/maps/Spandau/global_ENG.dsc")]);
        assert_eq!(super::dsc_candidates(p, "FRA").len(), 2);
        assert_eq!(super::dsc_candidates(std::path::Path::new("/v/MAN_EN92_main.bus"), "DEU"), vec![std::path::PathBuf::from("/v/MAN_EN92_main_DEU.dsc")]);
    }

    use super::*;

    /// A duty file written before the number plate field (or one that leaves it out) loads
    /// with no plate, and a plate the player typed is kept as it stands.
    #[test]
    fn a_picked_trip_starts_the_rest_of_the_tour() {
        let d = Duty { map: "maps/x/global.cfg".into(), bus: "Vehicles/x.bus".into(), time: "09:43".into(), line: Some("14".into()), tour: Some("1".into()), trip: Some("5".into()), whole_tour: true, ..Default::default() };
        let a = duty_args(&d).unwrap();
        let k = a.iter().position(|x| x == "--trip").unwrap();
        assert_eq!((a[k + 1].as_str(), a[k + 2].as_str()), ("5", "--whole-tour"));
        let alone = duty_args(&Duty { whole_tour: false, ..d }).unwrap();
        assert!(!alone.iter().any(|x| x == "--whole-tour"));
    }

    #[test]
    fn a_duty_keeps_its_plate_and_older_files_load_without_one() {
        let old: Duty = serde_json::from_str(r#"{"map":"maps/x/global.cfg","bus":"Vehicles/x.bus","time":"09:00"}"#).unwrap();
        assert_eq!(old.plate, None);
        let typed: Duty = serde_json::from_str(r#"{"map":"maps/x/global.cfg","bus":"Vehicles/x.bus","time":"09:00","plate":"B-AB 1234"}"#).unwrap();
        assert_eq!(typed.plate.as_deref(), Some("B-AB 1234"));
    }

    /// The fleet number picked in the launcher reaches the game.
    #[test]
    fn a_duty_passes_its_fleet_number() {
        let d: Duty = serde_json::from_str(r#"{"map":"maps/x/global.cfg","bus":"Vehicles/x.bus","time":"09:00","number":"4711"}"#).unwrap();
        let a = duty_args(&d).unwrap();
        assert!(a.windows(2).any(|w| w[0] == "--number" && w[1] == "4711"), "{a:?}");
    }

    #[test]
    fn portuguese_variants_are_distinct() {
        assert_eq!(language_code("pt-BR"), "PTB");
        assert_eq!(language_iso("PTB"), "pt");
        assert_eq!(language_code("pt-PT"), "PTP");
        assert_eq!(language_iso("PTP"), "pt-pt");
    }

    #[test]
    fn mirror_rendering_can_be_disabled() {
        let v = settings_from_text(Some("mirror_size=0\n"));
        assert_eq!(v["mirror_size"], 0);
        let text = settings_to_text(&v, None);
        assert!(text.lines().any(|l| l == "mirror_size=0"), "{text}");
    }

    #[test]
    fn mirror_refresh_survives_the_launcher() {
        assert_eq!(settings_from_text(None)["mirror_refresh"], "full");
        for mode in ["off", "eco", "full"] {
            let v = settings_from_text(Some(&format!("mirror_refresh={mode}\n")));
            assert_eq!(v["mirror_refresh"], mode);
            let text = settings_to_text(&v, None);
            assert!(text.lines().any(|l| l == format!("mirror_refresh={mode}")), "{text}");
        }
    }

    #[test]
    fn sixteen_x_anisotropy_survives_the_launcher() {
        let v = settings_from_text(Some("anisotropy=16\n"));
        assert_eq!(v["anisotropy"], 16);
        let text = settings_to_text(&v, None);
        assert!(text.lines().any(|l| l == "anisotropy=16"), "{text}");
        assert_eq!(settings_from_text(Some("anisotropy=32\n"))["anisotropy"], 16);
    }

    #[test]
    fn update_settings_round_trip() {
        // no file: look for updates, ask before installing
        let d = settings_from_text(None);
        assert_eq!((d["update_check"].clone(), d["update_auto"].clone()), (json!(true), json!(false)));
        let v = settings_from_text(Some("update_check=0\nupdate_auto=1\n"));
        assert_eq!((v["update_check"].clone(), v["update_auto"].clone()), (json!(false), json!(true)));
        let text = settings_to_text(&v, None);
        assert!(text.lines().any(|l| l == "update_check=0") && text.lines().any(|l| l == "update_auto=1"), "{text}");
    }

    #[test]
    fn vr_settings_round_trip() {
        let mut settings = settings_from_text(None);
        settings["vr"] = json!(true);
        settings["vr_scale"] = json!(0.8);
        settings["vr_head_smoothing_ms"] = json!(10);
        settings["vr_mirror_rate"] = json!(0);
        settings["vr_desktop_mirror"] = json!(false);
        let saved = settings_to_text(&settings, None);
        let loaded = settings_from_text(Some(&saved));
        for key in ["vr", "vr_scale", "vr_head_smoothing_ms", "vr_mirror_rate", "vr_desktop_mirror"] {
            assert_eq!(loaded[key], settings[key], "{key} was not saved");
        }
    }

    #[test]
    fn triple_screen_settings_survive_launcher_save() {
        let input = "triple_screen=1\ntriple_span=0\ntriple_width_mm=620\ntriple_distance_mm=700\ntriple_bezel_mm=18\ntriple_left_angle_deg=50\ntriple_right_angle_deg=40\ntriple_eye_height_mm=60\n";
        let values = settings_from_text(Some(input));
        let loaded = settings_from_text(Some(&settings_to_text(&values, Some(input))));
        for key in [
            "triple_screen",
            "triple_span",
            "triple_width_mm",
            "triple_distance_mm",
            "triple_bezel_mm",
            "triple_left_angle_deg",
            "triple_right_angle_deg",
            "triple_eye_height_mm",
        ] {
            assert_eq!(loaded[key], values[key], "{key}");
        }
        let invalid = settings_from_text(Some(
            "triple_width_mm=NaN\ntriple_distance_mm=inf\ntriple_bezel_mm=-20\n",
        ));
        assert_eq!(invalid["triple_width_mm"], 600.0);
        assert_eq!(invalid["triple_distance_mm"], 650.0);
        assert_eq!(invalid["triple_bezel_mm"], 0.0);
        assert_eq!(values["triple_hud_center"], true);
        let values = settings_from_text(Some(
            "triple_screen=1\ntriple_hud_center=0\ntriple_fov_deg=75\nfov=50\n",
        ));
        let loaded = settings_from_text(Some(&settings_to_text(&values, None)));
        assert_eq!(loaded["triple_hud_center"], false);
        assert_eq!(loaded["triple_fov_deg"], 75.0);
        assert_eq!(loaded["fov"], 50.0);
    }

    #[test]
    fn steering_view_settings_survive_the_launcher() {
        let values = settings_from_text(Some("steer_look=1\nsteer_look_angle=45\nsteer_look_response=0.5\n"));
        let saved = settings_to_text(&values, Some("steer_look_angle=10\nsteer_look_response=0.1\n"));
        let loaded = settings_from_text(Some(&saved));
        assert_eq!(loaded["steer_look"], json!(true));
        assert_eq!(loaded["steer_look_angle"], json!(45.0));
        assert_eq!(loaded["steer_look_response"], json!(0.5));
        assert_eq!(saved.lines().filter(|l| l.starts_with("steer_look_angle=")).count(), 1);
        let invalid = settings_from_text(Some("steer_look_angle=NaN\nsteer_look_response=NaN\n"));
        assert_eq!(invalid["steer_look_angle"], json!(30.0));
        assert_eq!(invalid["steer_look_response"], json!(0.25));
    }

    #[test]
    fn high_vr_mirror_rates_survive_launcher_settings() {
        for rate in [-1, 0, 16, 60, 120, 240, 360] {
            let mut settings = settings_from_text(None);
            // Select controls store their values as strings.
            settings["vr_mirror_rate"] = json!(rate.to_string());
            let saved = settings_to_text(&settings, None);
            let loaded = settings_from_text(Some(&saved));
            assert_eq!(loaded["vr_mirror_rate"], json!(rate));
        }
        assert_eq!(settings_from_text(Some("vr_mirror_rate=NaN\n"))["vr_mirror_rate"], json!(16));
        assert_eq!(settings_from_text(Some("vr_mirror_rate=999\n"))["vr_mirror_rate"], json!(360));
    }

    /// The interface size: 100% without a file, kept as set, and a hand-written value out
    /// of range brought back into it.
    #[test]
    fn interface_size_round_trip() {
        assert_eq!(settings_from_text(None)["ui_scale"], json!(1.0));
        let mut v = settings_from_text(None);
        v["ui_scale"] = json!(1.5);
        let text = settings_to_text(&v, None);
        assert!(text.lines().any(|l| l == "ui_scale=1.5"), "{text}");
        assert_eq!(settings_from_text(Some(&text))["ui_scale"], json!(1.5));
        assert_eq!(settings_from_text(Some("ui_scale=9\n"))["ui_scale"], json!(2.0));
        assert_eq!(settings_from_text(Some("ui_scale=0.1\n"))["ui_scale"], json!(0.5));
        assert_eq!(settings_from_text(Some("ui_scale=big\n"))["ui_scale"], json!(1.0));
        // (growing with the window: on unless switched off, and kept)
        assert_eq!(settings_from_text(None)["ui_scale_window"], json!(true));
        let off = settings_from_text(Some("ui_scale_window=0\n"));
        assert_eq!(off["ui_scale_window"], json!(false));
        assert!(settings_to_text(&off, None).lines().any(|l| l == "ui_scale_window=0"));
        // (the notes in the corner: on unless switched off, and kept)
        assert_eq!(settings_from_text(None)["notes"], json!(true));
        assert!(settings_to_text(&settings_from_text(Some("notes=0\n")), None).lines().any(|l| l == "notes=0"));
    }

    /// The opacity is the whole interface's now: a file of an older build, where it was the
    /// navigator's, keeps its value under the new name, written once.
    #[test]
    fn the_navigators_opacity_becomes_the_interfaces() {
        let old = "navigator_opacity=0.5\n";
        let v = settings_from_text(Some(old));
        assert_eq!(v["ui_opacity"], json!(0.5));
        let text = settings_to_text(&v, Some(old));
        assert!(text.lines().any(|l| l == "ui_opacity=0.5"), "{text}");
        assert!(!text.contains("navigator_opacity"), "{text}");
    }

    #[test]
    fn settings_keep_what_the_page_does_not_manage() {
        // the user's file: a key of a newer game, a hand-written one, and the game's other
        // spellings of keys the page writes
        let old = "# mine\nversion=2\nmsaa=1\nview_distance=1500\nlang=de\ntexmemlimit=401.0\nfuture_switch=7\nOMSI_Thing = on\nfractal=0\n";
        let v = settings_from_text(Some(old));
        assert_eq!(v["msaa"], 1);
        assert_eq!(v["view_distance"], "1500");
        assert_eq!(v["language"], "DEU");
        assert_eq!(v["texture_memory"], 401);
        assert_eq!(v["texture_compression"], true);
        assert_eq!(v["detail_textures"], false);
        // the page changes a few things (its selects give strings) and saves
        let mut page = v.clone();
        page["view_distance"] = json!("2000");
        page["language"] = json!("FRA");
        page["texture_memory"] = json!("3000");
        page["texture_compression"] = json!(false);
        page["texture_memory_auto"] = json!(2000);
        let text = settings_to_text(&page, Some(old));
        for line in ["view_distance=2000", "language=FRA", "texture_memory=3000", "texture_compression=0", "detail_textures=0", "future_switch=7", "OMSI_Thing = on"] {
            assert!(text.lines().any(|l| l == line), "{line} missing in\n{text}");
        }
        // the old spellings would override what was just written: gone, and nothing twice
        for gone in ["lang=", "texmemlimit=", "fractal=", "texture_memory_auto", "view_distance=1500"] {
            assert!(!text.contains(gone), "{gone} kept in\n{text}");
        }
        let keys: Vec<&str> = text.lines().filter_map(|l| l.split_once('=')).map(|(k, _)| k.trim()).collect();
        let mut unique = keys.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(keys.len(), unique.len(), "a key written twice:\n{text}");
        // and it reads back as saved
        let back = settings_from_text(Some(&text));
        assert_eq!(back["view_distance"], "2000");
        assert_eq!(back["language"], "FRA");
        assert_eq!(back["texture_memory"], 3000);
        assert_eq!(back["texture_compression"], false);
    }

    /// The lines follow the date as the game's do: Spandau's 1991 timetable change takes
    /// "5 & 5N" off and brings "130 & N30"; without a date it is the game's default day.
    #[test]
    fn lines_follow_the_chrono_date() {
        let root = std::env::var_os("OMSI_ROOT").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("../../../OMSI 2 Original"));
        let map = root.join("maps/Berlin-Spandau");
        if !map.join("TTData").is_dir() {
            eprintln!("skipped: no {}", map.display());
            return;
        }
        let names = |date: &str| lines_on(&map, date).unwrap().into_iter().map(|l| l.name).collect::<Vec<_>>();
        let (then, now) = (names(""), names("2026-09-17"));
        assert_eq!(then, names(DEFAULT_DATE));
        assert!(then.iter().any(|l| l == "5 & 5N") && !then.iter().any(|l| l == "130 & N30"), "{then:?}");
        assert!(!now.iter().any(|l| l == "5 & 5N") && now.iter().any(|l| l == "130 & N30"), "{now:?}");
        assert!(names("1991-06-01").iter().any(|l| l == "5 & 5N") && !names("1991-06-02").iter().any(|l| l == "5 & 5N"));
        assert!(lines_on(&map, "someday").is_err());
    }

    #[test]
    fn settings_defaults_and_automatic_values() {
        let v = settings_from_text(None);
        assert_eq!(v["view_distance"], "auto");
        assert_eq!(v["language"], "ENG");
        assert_eq!(v["texture_memory"], 0);
        let text = settings_to_text(&v, None);
        for line in ["view_distance=auto", "language=ENG", "texture_memory=0", "texture_compression=1", "render_scale=auto"] {
            assert!(text.lines().any(|l| l == line), "{line} missing in\n{text}");
        }
        // nonsense from a hand-edited file falls back to the defaults
        let v = settings_from_text(Some("view_distance=far\nview_distance=-5\nlanguage=Klingon\ntexture_memory=lots\n"));
        assert_eq!((v["view_distance"].as_str(), v["language"].as_str(), v["texture_memory"].as_i64()), (Some("auto"), Some("ENG"), Some(0)));
        // a number from a script instead of the select's string
        let text = settings_to_text(&json!({ "view_distance": 900, "texture_memory": 1500.0 }), None);
        assert!(text.contains("\nview_distance=900\n") && text.contains("\ntexture_memory=1500\n"), "{text}");
        #[cfg(unix)]
        assert!(physical_memory().unwrap_or(0) > 256_000_000, "the machine's memory is read");
    }
}

/// The machine's local date and time: (year, month, day, hour, minute).
#[cfg(unix)]
pub fn local_now() -> Option<(i32, i32, i32, i32, i32)> {
    // SAFETY: time and localtime_r only write the struct handed to them
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return None;
        }
        Some((tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min))
    }
}

#[cfg(windows)]
pub fn local_now() -> Option<(i32, i32, i32, i32, i32)> {
    // SAFETY: GetLocalTime only fills the struct handed to it
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    Some((t.wYear as i32, t.wMonth as i32, t.wDay as i32, t.wHour as i32, t.wMinute as i32))
}

#[cfg(not(any(unix, windows)))]
pub fn local_now() -> Option<(i32, i32, i32, i32, i32)> {
    None
}

#[cfg(test)]
mod omsi_options_tests {
    #[test]
    fn the_originals_options_are_read() {
        let root = std::path::Path::new("../../../OMSI 2 Original");
        let Some(o) = super::omsi_options(root) else { return };
        assert_eq!(o.last_map.as_deref(), Some("maps/Berlin-Spandau/global.cfg"));
        assert_eq!(o.last_driver.as_deref(), Some("OMSI-Fan"));
        assert_eq!(o.settings["max_fps"], 30);
        assert_eq!(o.settings["mirror_size"], 512);
        assert_eq!(o.settings["language"], "ENG");
        assert_eq!(o.settings["head_movement"], true);
        assert_eq!(o.settings["collision_vehicles"], false);
    }

    #[test]
    fn the_real_time_reflections_are_read_as_omsi_writes_them() {
        let root = std::env::temp_dir().join(format!("omsi-realrefl-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for (word, mode) in [("none", "off"), ("economy", "eco"), ("full", "full")] {
            std::fs::write(root.join("options.cfg"), format!("[performance_realreflexions]\r\n{word}\r\n\r\n[performance_reflTexSize]\r\n9\r\n")).unwrap();
            let o = super::omsi_options(&root).unwrap();
            assert_eq!(o.settings["mirror_refresh"], mode, "{word}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
