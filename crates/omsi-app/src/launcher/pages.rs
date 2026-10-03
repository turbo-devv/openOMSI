//! The launcher's other pages: the driver's profile, the settings, the key bindings, the
//! running games, the mods and where things are.

use super::state::{fmt_bytes, hhmm, short_map};
use super::theme::*;
use super::ui::{id_of, ButtonKind, Ui};
use super::{Launcher, Page};
use glam::Vec2;
use omsi_launcher_lib as core;
use omsi_ui::paint::Align;
use omsi_ui::{Color, Rect, Weight};
use serde_json::{json, Value};

#[derive(Default)]
pub struct PagesView {
    pub new_driver: String,
    pub confirm_delete: Option<std::time::Instant>,
    /// The "reset every setting" dialog is open.
    pub confirm_reset: bool,
    pub kb_filter: [String; 2],
    /// (section, index) of the binding waiting for a key.
    pub capturing: Option<(usize, usize)>,
    pub drop_hover: bool,
    pub setup_root: Option<String>,
    pub setup_game: Option<String>,
    /// The Controls page's tab: 0 the keyboard, 1 the game controllers.
    pub controls_tab: usize,
    /// The Settings page's tab (see `SETTINGS_TABS`).
    pub settings_tab: usize,
    pub pads: PadsView,
    pub tt: super::timetable::TimetableView,
}

/// The game controllers tab: the devices `gamectrler.cfg` sets up, the ones connected now,
/// the one shown, a button being waited for.
#[derive(Default)]
pub struct PadsView {
    pub io: Option<crate::controllers::Devices>,
    /// The set-up assistant, while it runs.
    pub wizard: Option<Wizard>,
    feedback_test: bool,
    pub devices: Option<Vec<crate::controllers::DeviceCfg>>,
    pub selected: usize,
    /// Waiting for a button of the shown device to be pressed (to add its binding).
    pub capturing: bool,
    /// A button found through "Add a button", kept visible even past the highlight.
    pub revealed_button: Option<usize>,
    pub dirty: bool,
    /// The button last pressed on the shown device and when: its line is lit, so that one
    /// sees which it is and what it does, and can give it an action there.
    pub last_pressed: Option<(usize, std::time::Instant)>,
}

/// The set-up assistant of a device: the player lets go of everything, then turns the wheel
/// to the left and presses each pedal in turn; what moved most each time is that control
/// (and which way it runs), as OMSI's options dialog has the player choose by hand.
pub struct Wizard {
    pub step: usize,
    /// Where each axis rests, and where it stood at each step (left, throttle, brake,
    /// clutch).
    pub rest: [Option<f32>; 8],
    pub at: Vec<[Option<f32>; 8]>,
    pub error: Option<String>,
    calibration: Option<(std::time::Instant, crate::ffb_calibration::Calibration)>,
    ff_choice: Option<bool>,
    test_strength: f32,
}

impl PadsView {
    pub(super) fn cancel_feedback_test(&mut self) {
        release_feedback(&mut self.io, &mut self.feedback_test);
        if let Some((_, test)) = self.wizard.as_mut().and_then(|w| w.calibration.as_mut()) {
            if test.result.is_none() {
                test.fail("The test was interrupted. Please try again.");
            }
        }
    }

    /// Give every controller handle up before handing the hardware to the game. The normal
    /// controller list uses non-exclusive DirectInput too, not only the force-feedback test.
    pub(super) fn release_io(&mut self) {
        self.cancel_feedback_test();
        self.io = None;
    }
}

fn release_feedback(io: &mut Option<crate::controllers::Devices>, active: &mut bool) {
    if *active {
        *io = None;
        *active = false;
    }
}

// --- profile --------------------------------------------------------------------------------

pub fn profile(l: &mut Launcher, area: Rect) {
    let body = l.page_title(area, "Profile", "The driver whose personnel file the game writes: hours, kilometres, punctuality, tickets.");
    let left_w = (body.w * 0.56).min(700.0);
    let left = Rect::new(body.x, body.y, left_w, body.h);
    let right = Rect::new(body.x + left_w + GAP * 2.0, body.y, body.w - left_w - GAP * 2.0, body.h);
    // driver chooser
    let top = Rect::new(left.x, left.y, left.w, 132.0);
    l.ui.panel(top);
    let inner = l.ui.heading(Rect::new(top.x + 18.0, top.y + 14.0, top.w - 36.0, top.h - 28.0), "Driver", Some("person"));
    let names = l.state.profiles.clone();
    let mut sel = names.iter().position(|n| *n == l.state.config.profile).unwrap_or(0);
    let half = (inner.w - GAP) * 0.5;
    if !names.is_empty() && l.ui.select("profile", Rect::new(inner.x, inner.y, half, ROW), &mut sel, &names) {
        l.state.config.profile = names[sel].clone();
        let _ = core::save_config(&l.state.config);
        l.state.load_profile();
        l.state.touched();
    }
    let danger_armed = l.pages.confirm_delete.map(|t| t.elapsed().as_secs() < 4).unwrap_or(false);
    if l.ui.button("profile-delete", Rect::new(inner.x + half + GAP, inner.y, half, ROW), if danger_armed { "Click again to delete" } else { "Delete this driver" }, Some("delete"), ButtonKind::Danger) {
        if danger_armed {
            let name = l.state.config.profile.clone();
            match core::delete_profile(&name) {
                Ok(()) => {
                    l.state.set_status(format!("Personnel file of {name} deleted."), false);
                    l.state.load_profiles();
                }
                Err(e) => l.state.set_status(format!("{e:#}"), true),
            }
            l.pages.confirm_delete = None;
        } else {
            l.pages.confirm_delete = Some(std::time::Instant::now());
        }
    }
    let y = inner.y + ROW + 10.0;
    l.ui.text_input("new-driver", Rect::new(inner.x, y, half, ROW), &mut l.pages.new_driver, "New driver's name", Some("person"));
    if l.ui.button("profile-create", Rect::new(inner.x + half + GAP, y, half, ROW), "Create", Some("add"), ButtonKind::Normal) {
        let name = l.pages.new_driver.trim().to_string();
        if !name.is_empty() {
            match core::create_profile(&name, "M") {
                Ok(_) => {
                    l.state.config.profile = name.clone();
                    let _ = core::save_config(&l.state.config);
                    l.pages.new_driver.clear();
                    l.state.load_profiles();
                    l.state.set_status(format!("Driver {name} created."), false);
                }
                Err(e) => l.state.set_status(format!("{e:#}"), true),
            }
        }
    }
    // level and stats
    let card = Rect::new(left.x, top.bottom() + GAP * 1.5, left.w, left.h - top.h - GAP * 1.5);
    l.ui.panel(card);
    let Some(p) = l.state.profile.clone() else {
        l.ui.text_in("Create a driver to start a personnel file.", card.pad(20.0, 20.0), 14.0, Weight::Medium, TEXT_DIM, Align::Left);
        return;
    };
    let c = Vec2::new(card.x + 70.0, card.y + 76.0);
    let prev = ((p.level - 1) * (p.level - 1) * 250) as f64;
    let frac = ((p.xp as f64 - prev) / (p.next_level_xp as f64 - prev).max(1.0)).clamp(0.0, 1.0) as f32;
    let shown = l.ui.anim(id_of("xp-ring"), frac, 0.6);
    l.ui.p().circle(c, 50.0, Color::rgba(28, 31, 37, 1.0));
    l.ui.p().arc(c, 44.0, 52.0, 0.0, std::f32::consts::TAU, Color::WHITE.alpha(0.08));
    let a0 = -std::f32::consts::FRAC_PI_2;
    l.ui.p().arc(c, 44.0, 52.0, a0, a0 + std::f32::consts::TAU * shown.max(0.01), ACCENT);
    l.ui.text_in(&p.level.to_string(), Rect::new(c.x - 40.0, c.y - 26.0, 80.0, 40.0), 34.0, Weight::Black, TEXT, Align::Center);
    l.ui.text_in("LEVEL", Rect::new(c.x - 40.0, c.y + 12.0, 80.0, 16.0), 10.0, Weight::Black, TEXT_DIM, Align::Center);
    let tx = card.x + 142.0;
    l.ui.text_in(&format!("{}{}", p.name, if p.exists { "" } else { " (no personnel file yet)" }), Rect::new(tx, card.y + 34.0, card.w - 160.0, 30.0), 24.0, Weight::Black, TEXT, Align::Left);
    l.ui.progress(Rect::new(tx, card.y + 76.0, card.w - 170.0, 10.0), shown, false);
    l.ui.text_in(&format!("{} XP · {} to level {}", p.xp, (p.next_level_xp - p.xp).max(0), p.level + 1), Rect::new(tx, card.y + 94.0, card.w - 160.0, 18.0), 12.5, Weight::Medium, TEXT_DIM, Align::Left);
    let hours = |h: f64| format!("{} h {:02} min", h.floor() as i64, ((h - h.floor()) * 60.0).round() as i64);
    let stats = [
        ("schedule", hours(p.hours), "hours driven"),
        ("route", format!("{:.1} km", p.km), "distance"),
        ("location_on", p.stops.to_string(), "stops served"),
        ("timer", format!("{} / {}", p.early, p.late), "early / late"),
        ("confirmation_number", format!("{:.0}", p.tickets), "tickets sold"),
        ("payments", format!("{:.2}", p.cash), "takings"),
        ("warning", p.crashes.to_string(), "crashes"),
        ("person", p.hurt.to_string(), "pedestrians hurt"),
        ("speed", format!("{:.0} %", p.rating_driving), "driving"),
        ("airport_shuttle", format!("{:.0} %", p.rating_comfort), "comfort"),
        ("receipt_long", format!("{:.0} %", p.rating_tickets), "ticket selling"),
        ("history", p.sessions.len().to_string(), "runs"),
    ];
    let grid = Rect::new(card.x + 18.0, card.y + 150.0, card.w - 36.0, card.h - 170.0);
    let cols = 3;
    let cw = (grid.w - GAP * (cols as f32 - 1.0)) / cols as f32;
    let ch = 68.0;
    for (k, (icon, v, label)) in stats.iter().enumerate() {
        let (cx, cy) = ((k % cols) as f32, (k / cols) as f32);
        let r = Rect::new(grid.x + cx * (cw + GAP), grid.y + cy * (ch + 10.0), cw, ch);
        if r.bottom() > card.bottom() - 6.0 {
            break;
        }
        l.ui.p().rounded(r, 10.0, Color::WHITE.alpha(0.04));
        l.ui.icon(icon, Vec2::new(r.x + 22.0, r.y + 22.0), 18.0, ACCENT);
        l.ui.text_in(v, Rect::new(r.x + 40.0, r.y + 8.0, r.w - 48.0, 28.0), 18.0, Weight::Black, TEXT, Align::Left);
        l.ui.text_in(label, Rect::new(r.x + 14.0, r.y + 42.0, r.w - 20.0, 18.0), 11.5, Weight::Medium, TEXT_DIM, Align::Left);
    }
    // recent runs
    l.ui.panel(right);
    let inner = l.ui.heading(Rect::new(right.x + 18.0, right.y + 14.0, right.w - 36.0, right.h - 28.0), "Recent runs", Some("history"));
    let sessions = p.sessions.clone();
    l.ui.scroll_area("runs", Rect::new(inner.x - 6.0, inner.y, inner.w + 12.0, inner.h), &mut |ui, v| {
        if sessions.is_empty() {
            ui.text_in("No runs yet. Drive a duty and it shows up here.", Rect::new(v.x + 8.0, v.y, v.w, 30.0), 13.0, Weight::Regular, TEXT_DIM, Align::Left);
            return 30.0;
        }
        let rh = 62.0;
        for (k, s) in sessions.iter().enumerate() {
            let r = Rect::new(v.x + 6.0, v.y + k as f32 * rh, v.w - 16.0, rh - 6.0);
            ui.p().rounded(r, 9.0, Color::WHITE.alpha(0.04));
            let title = match &s.line {
                Some(line) => format!("Line {line}{} · {}", s.tour.as_ref().map(|t| format!(" / {t}")).unwrap_or_default(), short_map(&s.map)),
                None => format!("Free drive · {}", short_map(&s.map)),
            };
            ui.text_in(&title, Rect::new(r.x + 12.0, r.y + 6.0, r.w - 130.0, 20.0), 13.0, Weight::Bold, TEXT, Align::Left);
            ui.text_in(&format!("{} · {:.1} km · {} stops · {} tickets · {} crashes", s.bus.rsplit('/').next().unwrap_or(""), s.metres / 1000.0, s.stops, s.tickets, s.crashes), Rect::new(r.x + 12.0, r.y + 28.0, r.w - 130.0, 18.0), 11.0, Weight::Regular, TEXT_DIM, Align::Left);
            let when = chrono_like(s.time);
            ui.text_in(&when, Rect::new(r.right() - 120.0, r.y + 6.0, 110.0, 20.0), 11.5, Weight::Medium, TEXT_SOFT, Align::Right);
            ui.text_in(&hours_short(s.seconds / 3600.0), Rect::new(r.right() - 120.0, r.y + 28.0, 110.0, 18.0), 11.5, Weight::Medium, ACCENT, Align::Right);
        }
        sessions.len() as f32 * rh
    });
}

fn hours_short(h: f64) -> String {
    format!("{}:{:02} h", h.floor() as i64, ((h - h.floor()) * 60.0).round() as i64)
}

/// A Unix time as "YYYY-MM-DD HH:MM" in the machine's time zone.
fn chrono_like(t: u64) -> String {
    #[cfg(unix)]
    {
        let tt = t as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        if !unsafe { libc::localtime_r(&tt, &mut tm) }.is_null() {
            return format!("{:04}-{:02}-{:02} {:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min);
        }
    }
    let days = (t / 86400) as i64;
    let secs = t % 86400;
    // civil from days (Howard Hinnant), UTC
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", secs / 3600, (secs % 3600) / 60)
}

// --- settings ---------------------------------------------------------------------------------

/// A value of the settings as the pages show it.
fn get<'a>(v: &'a Value, k: &str) -> &'a Value {
    v.get(k).unwrap_or(&Value::Null)
}

/// The window sizes the settings offer (`resolution`).
pub(crate) const RESOLUTIONS: &[(&str, &str)] = &[("auto", "Automatic"), ("1280x720", "1280 x 720"), ("1280x800", "1280 x 800 (Steam Deck)"), ("1366x768", "1366 x 768"), ("1600x900", "1600 x 900"), ("1920x1080", "1920 x 1080"), ("1920x1200", "1920 x 1200"), ("2560x1440", "2560 x 1440"), ("3840x2160", "3840 x 2160")];

fn sel_setting(ui: &mut Ui, s: &mut Value, dirty: &mut f32, name: &str, r: Rect, label: &str, key: &str, options: &[(&str, &str)]) {
    ui.label(Rect::new(r.x, r.y, r.w * 0.45, r.h), label);
    let cur = match get(s, key) {
        Value::String(x) => x.clone(),
        Value::Bool(b) => (*b as u8).to_string(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    };
    let mut labels: Vec<String> = options.iter().map(|o| o.1.to_string()).collect();
    let mut values: Vec<String> = options.iter().map(|o| o.0.to_string()).collect();
    let mut sel = values.iter().position(|v| *v == cur || v.parse::<f64>().ok().zip(cur.parse::<f64>().ok()).map(|(a, b)| (a - b).abs() < 1e-6).unwrap_or(false));
    if sel.is_none() && !cur.is_empty() {
        // a value written by hand gets an entry of its own
        labels.push(cur.clone());
        values.push(cur.clone());
        sel = Some(values.len() - 1);
    }
    let mut sel = sel.unwrap_or(0);
    if ui.select(name, Rect::new(r.x + r.w * 0.45, r.y, r.w * 0.55, r.h), &mut sel, &labels) {
        let v = &values[sel];
        s[key] = match get(s, key) {
            Value::Bool(_) => json!(v == "1"),
            Value::Number(_) => v.parse::<f64>().map(|f| if f.fract() == 0.0 { json!(f as i64) } else { json!(f) }).unwrap_or(json!(v)),
            _ => json!(v),
        };
        *dirty = 0.3;
    }
}

fn toggle_setting(ui: &mut Ui, s: &mut Value, dirty: &mut f32, r: Rect, label: &str, key: &str) {
    let mut v = get(s, key).as_bool().unwrap_or(false);
    if ui.toggle(&format!("set-{key}"), r, &mut v, label) {
        s[key] = json!(v);
        *dirty = 0.3;
    }
}

/// The settings page's tabs: what one has come to change.
pub const SETTINGS_TABS: [&str; 6] = ["Graphics", "Driving", "Camera", "Sound", "Gameplay", "General"];

/// What the tabs show of the launcher and ask of it (they see only the settings): the
/// updater's state, "Check now", "Reset all settings", the Controls page at one of its tabs.
struct Outside {
    update: crate::updater::Status,
    check_updates: bool,
    reset: bool,
    controls: Option<usize>,
}

thread_local! {
    /// How high each tab's two columns came out (the stacked layout's heights, and how far
    /// the page scrolls).
    static SETTINGS_COL_H: std::cell::Cell<[[f32; 2]; SETTINGS_TABS.len()]> = const { std::cell::Cell::new([[0.0; 2]; SETTINGS_TABS.len()]) };
}

pub fn settings(l: &mut Launcher, area: Rect) {
    let body = l.page_title(area, "Settings", "Every change is saved at once; the game reads them when it starts.");
    // (one tab at a time: the whole page in three columns was taller than two screens, and a
    // setting was found by reading all of it)
    let bar = Rect::new(body.x, body.y, body.w.min(660.0), 36.0);
    let mut tab = l.pages.settings_tab;
    if l.ui.segmented("settings-tab", bar, &mut tab, &SETTINGS_TABS) {
        l.pages.settings_tab = tab;
    }
    let body = Rect::new(body.x, bar.bottom() + 18.0, body.w, (body.bottom() - bar.bottom() - 18.0).max(0.0));
    let s = &mut l.state.settings;
    let dirty = &mut l.state.settings_dirty;
    let mut out = Outside { update: l.update.status(), check_updates: false, reset: false, controls: None };
    // (two columns side by side; where they would be too narrow to read - a phone - one
    // under the other, each as high as it was the frame before)
    let stacked = body.w < 900.0;
    // (each tab scrolled where it was left, not where another one was)
    l.ui.scroll_area(&format!("settings-page-{tab}"), body, &mut |ui, v| {
        let hs = SETTINGS_COL_H.with(|c| c.get())[tab];
        let w = v.w - 8.0;
        let (cols, h) = if stacked {
            ([Rect::new(v.x, v.y, w, hs[0]), Rect::new(v.x, v.y + hs[0] + GAP * 2.0, w, hs[1])], hs[0] + hs[1] + GAP * 2.0)
        } else {
            let cw = (w - GAP * 2.0) / 2.0;
            let h = v.h.max(hs[0]).max(hs[1]);
            ([Rect::new(v.x, v.y, cw, h), Rect::new(v.x + cw + GAP * 2.0, v.y, cw, h)], h)
        };
        let used = settings_tab(ui, tab, s, dirty, &mut out, cols);
        SETTINGS_COL_H.with(|c| {
            let mut all = c.get();
            all[tab] = used;
            c.set(all);
        });
        h
    });
    if out.check_updates {
        l.update.check();
    }
    if out.reset {
        l.pages.confirm_reset = true;
    }
    if let Some(t) = out.controls {
        l.pages.controls_tab = t;
        l.go(Page::Controls);
    }
}

/// The dialog that asks before every setting goes back to how it came.
pub fn reset_dialog(l: &mut Launcher) {
    let size = l.ui.size;
    let full = Rect::new(0.0, 0.0, size.x, size.y);
    l.ui.solid(full);
    l.ui.p().rect(full, omsi_ui::Color::rgba(0, 0, 0, 0.62));
    let w = (size.x - 48.0).min(520.0);
    let h = 190.0;
    let r = Rect::new((size.x - w) * 0.5, (size.y - h) * 0.5, w, h);
    l.ui.panel(r);
    let inner = Rect::new(r.x + 24.0, r.y + 20.0, r.w - 48.0, r.h - 40.0);
    l.ui.icon("restart_alt", Vec2::new(inner.x + 14.0, inner.y + 14.0), 26.0, DANGER);
    l.ui.text_in("Reset every setting?", Rect::new(inner.x + 38.0, inner.y, inner.w - 38.0, 28.0), 18.0, Weight::Bold, TEXT, Align::Left);
    l.ui.paragraph("Graphics, sound, controllers and game settings go back to how they came. The language, the drivers, the key bindings and the game folder stay.", Vec2::new(inner.x, inner.y + 40.0), inner.w, 13.0, Weight::Regular, TEXT_DIM);
    let by = inner.bottom() - 38.0;
    if l.ui.button("reset-no", Rect::new(inner.right() - 250.0, by, 110.0, 38.0), "Cancel", None, ButtonKind::Normal) {
        l.pages.confirm_reset = false;
    }
    if l.ui.button("reset-yes", Rect::new(inner.right() - 130.0, by, 130.0, 38.0), "Reset", Some("restart_alt"), ButtonKind::Danger) {
        let language = l.state.settings.get("language").cloned();
        l.state.settings = core::settings_from_text(None);
        if let Some(lang) = language {
            l.state.settings["language"] = lang;
        }
        l.state.settings_dirty = 0.3;
        l.pages.confirm_reset = false;
        l.state.set_status("Every setting is back to how it came.", false);
    }
}

/// A column of a settings tab: its panel, and the rows and sections laid down it.
struct Col {
    top: f32,
    inner: Rect,
    y: f32,
}

impl Col {
    /// The column in `r`, under the title of its first section.
    fn new(ui: &mut Ui, r: Rect, title: &str) -> Col {
        ui.panel(r);
        let inner = ui.heading(Rect::new(r.x + 18.0, r.y + 14.0, r.w - 36.0, r.h - 28.0), title, None);
        Col { top: r.y, inner, y: inner.y }
    }

    /// The next row.
    fn row(&mut self) -> Rect {
        let r = Rect::new(self.inner.x, self.y, self.inner.w, ROW - 2.0);
        self.y += ROW + 4.0;
        r
    }

    /// Another section, under its title.
    fn section(&mut self, ui: &mut Ui, title: &str) {
        self.y += 6.0;
        ui.heading(Rect::new(self.inner.x, self.y, self.inner.w, 28.0), title, None);
        self.y += 32.0;
    }

    /// How high the column came out.
    fn used(&self) -> f32 {
        self.y - self.top + 14.0
    }
}

/// Tab `tab` of the settings in its two columns. Returns the height each column needed (0
/// for one the tab leaves empty).
fn settings_tab(ui: &mut Ui, tab: usize, s: &mut Value, dirty: &mut f32, out: &mut Outside, cols: [Rect; 2]) -> [f32; 2] {
    match tab {
        0 => graphics_tab(ui, s, dirty, cols),
        1 => driving_tab(ui, s, dirty, out, cols),
        2 => camera_tab(ui, s, dirty, out, cols),
        3 => sound_tab(ui, s, dirty, cols),
        4 => gameplay_tab(ui, s, dirty, cols),
        _ => general_tab(ui, s, dirty, out, cols),
    }
}

/// The saved graphics profiles' part of the Graphics tab: the list, the name being typed.
#[derive(Default)]
struct GfxProfileUi {
    name: String,
    sel: usize,
    list: Option<Vec<String>>,
    msg: String,
}

thread_local! {
    static GFX_PROFILES: std::cell::RefCell<GfxProfileUi> = std::cell::RefCell::new(GfxProfileUi::default());
}

/// Save, load and delete the graphics settings as named profiles.
fn graphics_profiles_block(ui: &mut Ui, s: &mut Value, dirty: &mut f32, c: &mut Col) {
    GFX_PROFILES.with(|g| {
        let mut g = g.borrow_mut();
        let g = &mut *g;
        let names: Vec<String> = g.list.get_or_insert_with(|| core::graphics_profiles().into_keys().collect()).clone();
        g.sel = g.sel.min(names.len().saturating_sub(1));
        let labels: Vec<String> = if names.is_empty() { vec!["No saved profiles".to_string()] } else { names.clone() };
        let r = c.row();
        ui.label(Rect::new(r.x, r.y, r.w * 0.45, r.h), "Saved profile");
        if ui.select("s-gp-sel", Rect::new(r.x + r.w * 0.45, r.y, r.w * 0.55, r.h), &mut g.sel, &labels) && !names.is_empty() {
            g.name = names[g.sel].clone();
        }
        let r = c.row();
        let half = (r.w - GAP) * 0.5;
        if ui.button("s-gp-load", Rect::new(r.x, r.y, half, r.h), "Load", Some("download"), ButtonKind::Normal) && !names.is_empty() {
            let name = names[g.sel].clone();
            match core::graphics_profiles().get(&name) {
                Some(p) => {
                    core::apply_graphics_profile(p, s);
                    *dirty = 0.3;
                    g.msg = format!("Loaded \"{name}\".");
                }
                None => g.msg = format!("\"{name}\" is gone."),
            }
        }
        if ui.button("s-gp-del", Rect::new(r.x + half + GAP, r.y, half, r.h), "Delete", Some("delete"), ButtonKind::Danger) && !names.is_empty() {
            let name = names[g.sel].clone();
            g.msg = match core::delete_graphics_profile(&name) {
                Ok(()) => format!("Deleted \"{name}\"."),
                Err(e) => format!("{e:#}"),
            };
            g.list = None;
        }
        let r = c.row();
        ui.text_input("s-gp-name", r, &mut g.name, "Profile name", None);
        let r = c.row();
        if ui.button("s-gp-save", r, "Save current graphics as profile", Some("save"), ButtonKind::Primary) {
            g.msg = match core::save_graphics_profile(&g.name, s) {
                Ok(name) => {
                    g.name = name.clone();
                    g.list = None;
                    if let Some(i) = core::graphics_profiles().keys().position(|k| *k == name) {
                        g.sel = i;
                    }
                    format!("Saved \"{name}\".")
                }
                Err(e) => format!("{e:#}"),
            };
        }
        if !g.msg.is_empty() {
            c.y += ui.paragraph(&g.msg, Vec2::new(c.inner.x, c.y), c.inner.w, 12.5, Weight::Regular, TEXT_DIM) + 8.0;
        }
    });
}

/// How the game looks and how fast it runs.
fn graphics_tab(ui: &mut Ui, s: &mut Value, dirty: &mut f32, cols: [Rect; 2]) -> [f32; 2] {
    let mut c = Col::new(ui, cols[0], "Graphics");
    // Quality presets, first: they set most of what follows. (OMSI's own
    // option_presets/*.oop are named after the PCs of their day - "PC 2006", "X10 high",
    // "Chicago Recommended" - which read as random words here.)
    let presets: [(&str, serde_json::Value); 4] = [
        ("Low", json!({"msaa": 1, "anisotropy": 2, "shadow_size": 1024, "ssao": false, "shadows": false, "detail_textures": false, "clouds": false, "view_distance": "600", "min_obj_size": 0.03, "max_obj_dist": "500", "mirror_size": 128, "mirror_refresh": "eco", "render_scale": "0.75", "texture_memory": 800})),
        ("Medium", json!({"msaa": 2, "anisotropy": 4, "shadow_size": 2048, "ssao": false, "shadows": true, "detail_textures": true, "clouds": true, "view_distance": "900", "min_obj_size": 0.02, "max_obj_dist": "750", "mirror_size": 256, "mirror_refresh": "eco", "render_scale": "auto", "texture_memory": 1200})),
        ("High", json!({"msaa": 4, "anisotropy": 8, "shadow_size": 2048, "ssao": true, "shadows": true, "detail_textures": true, "clouds": true, "view_distance": "auto", "min_obj_size": 0.013, "max_obj_dist": "auto", "mirror_size": 256, "mirror_refresh": "full", "render_scale": "auto", "texture_memory": 0})),
        ("Ultra", json!({"msaa": 4, "anisotropy": 8, "shadow_size": 4096, "ssao": true, "shadows": true, "detail_textures": true, "clouds": true, "view_distance": "2000", "min_obj_size": 0.005, "max_obj_dist": "1500", "mirror_size": 512, "mirror_refresh": "full", "render_scale": "auto", "texture_memory": 0})),
    ];
    {
        // the preset the settings match now, else "Custom"
        let matches = |p: &serde_json::Value| p.as_object().map(|o| o.iter().all(|(k, v)| {
            let cur = get(s, k);
            cur == v || cur.as_f64().zip(v.as_f64()).map(|(a, b)| (a - b).abs() < 1e-6).unwrap_or(false) || cur.as_str().zip(v.as_f64()).map(|(a, b)| a.parse::<f64>().map(|a| (a - b).abs() < 1e-6).unwrap_or(false)).unwrap_or(false) || cur.as_f64().zip(v.as_str()).map(|(a, b)| b.parse::<f64>().map(|b| (a - b).abs() < 1e-6).unwrap_or(false)).unwrap_or(false)
        })).unwrap_or(false);
        let mut labels: Vec<String> = presets.iter().map(|p| p.0.to_string()).collect();
        labels.push("Custom".to_string());
        let mut sel = presets.iter().position(|p| matches(&p.1)).unwrap_or(presets.len());
        let r = c.row();
        ui.label(Rect::new(r.x, r.y, r.w * 0.45, r.h), "Quality preset");
        if ui.select("s-preset", Rect::new(r.x + r.w * 0.45, r.y, r.w * 0.55, r.h), &mut sel, &labels) && sel < presets.len() {
            if let Some(obj) = presets[sel].1.as_object() {
                for (k, v) in obj {
                    s[k.as_str()] = v.clone();
                }
                *dirty = 0.3;
            }
        }
    }
    sel_setting(ui, s, dirty, "s-graphics", c.row(), "Graphics", "graphics", &[("vanilla", "Vanilla (as OMSI 2)"), ("vanilla_plus", "Vanilla+"), ("enhanced", "Enhanced")]);
    // Vanilla draws what OMSI 2 draws: no sun shadows, ambient occlusion or detail grain
    let classic = get(s, "graphics").as_str() == Some("vanilla");
    sel_setting(ui, s, dirty, "s-msaa", c.row(), "Anti-aliasing", "msaa", &[("1", "Off"), ("2", "2x MSAA"), ("4", "4x MSAA"), ("8", "8x MSAA")]);
    sel_setting(ui, s, dirty, "s-scale", c.row(), "Render scale", "render_scale", &[("auto", "Auto"), ("1", "100%"), ("0.85", "85%"), ("0.75", "75%"), ("0.67", "67%"), ("0.5", "50%")]);
    sel_setting(ui, s, dirty, "s-af", c.row(), "Anisotropic", "anisotropy", &[("1", "Off"), ("2", "2x"), ("4", "4x"), ("8", "8x"), ("16", "16x")]);
    if !classic {
        sel_setting(ui, s, dirty, "s-shadow", c.row(), "Shadow map", "shadow_size", &[("1024", "1024"), ("2048", "2048"), ("4096", "4096")]);
        toggle_setting(ui, s, dirty, c.row(), "Ambient occlusion", "ssao");
        toggle_setting(ui, s, dirty, c.row(), "Sun shadows", "shadows");
        sel_setting(ui, s, dirty, "s-casters", c.row(), "Shadows cast by", "shadow_casters", &[("all", "Every solid mesh"), ("omsi", "[shadow] meshes, as OMSI")]);
        toggle_setting(ui, s, dirty, c.row(), "Detail texturing up close", "detail_textures");
        // (an LED panel's dots are its own light: how bright they burn, and how much of the
        // mip chain the panel's picture and its mask are held at - 0 point-samples them,
        // the sharpest dots and the worst shimmer; higher holds them at the level the
        // screen footprint asks for at most)
        let mut led = get(s, "led_glow").as_i64().unwrap_or(6) as f32;
        if ui.slider("s-led", c.row(), &mut led, 0.0, 15.0, 1.0, "LED glow", &|v| if v < 0.5 { "Off".to_string() } else { format!("{}", v as i64) }) {
            s["led_glow"] = json!(led.round() as i64);
            *dirty = 0.3;
        }
        let mut mip = get(s, "led_mips").as_f64().unwrap_or(1.3) as f32;
        if ui.slider("s-led-mip", c.row(), &mut mip, 0.0, 4.0, 0.05, "LED mip strength", &|v| if v < 0.005 { "Off".to_string() } else { format!("{v:.2}") }) {
            s["led_mips"] = json!((mip / 0.05).round() * 0.05);
            *dirty = 0.3;
        }
    }
    // (the models' `[isshadow]` blob is what OMSI draws under a vehicle in every graphics
    // mode, the vanilla one included, so its switch is not part of the extras above)
    toggle_setting(ui, s, dirty, c.row(), "OMSI's shadow meshes (under vehicles)", "shadow_blobs");
    toggle_setting(ui, s, dirty, c.row(), "Reflection maps (paint, chrome, glass)", "reflections");
    toggle_setting(ui, s, dirty, c.row(), "Clouds", "clouds");
    let left = c.used();
    let mut c = Col::new(ui, cols[1], "Display");
    toggle_setting(ui, s, dirty, c.row(), "Fullscreen", "fullscreen");
    // (the window's own size in pixels; a Steam Deck's Gaming Mode and other odd screens,
    // #904 - "Automatic" fits the screen, and fills it under gamescope)
    sel_setting(ui, s, dirty, "s-res", c.row(), "Window size", "resolution", RESOLUTIONS);
    toggle_setting(ui, s, dirty, c.row(), "V-sync", "vsync");
    sel_setting(ui, s, dirty, "s-fps", c.row(), "Frame limit", "max_fps", &[("0", "Screen refresh rate"), ("30", "30 fps"), ("45", "45 fps"), ("60", "60 fps"), ("120", "120 fps"), ("144", "144 fps"), ("1000", "Unlimited")]);
    // (a Mac has Metal only; elsewhere a driver's Vulkan that misbehaves, or a card without
    // it, is got round here)
    if cfg!(windows) {
        sel_setting(ui, s, dirty, "s-api", c.row(), "Graphics API", "graphics_api", &[("auto", "Automatic"), ("vulkan", "Vulkan"), ("dx12", "DirectX 12"), ("gl", "OpenGL")]);
    } else if !cfg!(target_os = "macos") {
        sel_setting(ui, s, dirty, "s-api", c.row(), "Graphics API", "graphics_api", &[("auto", "Automatic"), ("vulkan", "Vulkan"), ("gl", "OpenGL")]);
    }
    c.section(ui, "World & memory");
    sel_setting(ui, s, dirty, "s-view", c.row(), "View distance", "view_distance", &[("auto", "Default (1200 m)"), ("600", "600 m - fastest"), ("900", "900 m"), ("1200", "1200 m"), ("1500", "1500 m"), ("2000", "2000 m"), ("2500", "2500 m")]);
    sel_setting(ui, s, dirty, "s-maxobj", c.row(), "Object distance", "max_obj_dist", &[("auto", "Automatic"), ("500", "500 m"), ("750", "750 m"), ("900", "900 m"), ("1500", "1500 m"), ("3000", "3000 m")]);
    sel_setting(ui, s, dirty, "s-minobj", c.row(), "Small objects", "min_obj_size", &[("0.005", "All"), ("0.013", "Normal"), ("0.02", "Fewer (faster)"), ("0.03", "Few (fastest)")]);
    sel_setting(ui, s, dirty, "s-mirror", c.row(), "Mirrors", "mirror_size", &[("0", "Off"), ("128", "Low (128)"), ("256", "Normal (256)"), ("512", "High (512)"), ("1024", "Very high (1024)")]);
    sel_setting(ui, s, dirty, "s-mirror-refresh", c.row(), "Real-time reflections", "mirror_refresh", &[("off", "None (frozen picture)"), ("eco", "Economical"), ("full", "Full")]);
    // (the game takes the smaller of an eighth of the memory and what the graphics
    // adapter is taken to hold, see `memory::texture_budget`)
    let adapter_mb = omsi_render::ADAPTER_TEXTURE_MB.load(std::sync::atomic::Ordering::Relaxed) as i64;
    let auto_mb = match (get(s, "texture_memory_auto").as_i64().unwrap_or(0), adapter_mb) {
        (m, 0) => m,
        (0, a) => a,
        (m, a) => m.min(a),
    };
    let auto_label = if auto_mb > 0 { format!("Automatic ({} here)", mb(auto_mb)) } else { "Automatic".to_string() };
    let opts: Vec<(&str, &str)> = vec![("0", auto_label.as_str()), ("500", "500 MB"), ("1000", "1 GB"), ("1500", "1.5 GB"), ("2000", "2 GB"), ("3000", "3 GB"), ("4000", "4 GB"), ("6000", "6 GB")];
    sel_setting(ui, s, dirty, "s-texmem", c.row(), "Texture memory", "texture_memory", &opts);
    toggle_setting(ui, s, dirty, c.row(), "Compress textures on loading", "texture_compression");
    c.section(ui, "Profiles");
    graphics_profiles_block(ui, s, dirty, &mut c);
    [left, c.used()]
}

/// How the bus answers the keys, the mouse, a wheel and pedals (which key does what: the
/// Controls page).
fn driving_tab(ui: &mut Ui, s: &mut Value, dirty: &mut f32, out: &mut Outside, cols: [Rect; 2]) -> [f32; 2] {
    let mut c = Col::new(ui, cols[0], "Keyboard & mouse");
    sel_setting(ui, s, dirty, "s-keys", c.row(), "Driving keys", "drive_keys", &[("omsi", "Custom controls (Controls page)"), ("simple", "W A S D + arrows"), ("wasd", "W A S D only"), ("arrows", "Arrow keys only")]);
    toggle_setting(ui, s, dirty, c.row(), "Steering linearity (keys at OMSI's steady pace)", "steering_linear");
    toggle_setting(ui, s, dirty, c.row(), "Old Steering (the wheel stays, turn it back yourself)", "old_steering");
    toggle_setting(ui, s, dirty, c.row(), "Dynamic steering (slower keys at speed, OMSI's redSteerSpd)", "red_steer_spd");
    let mut ms = get(s, "mouse_sens").as_f64().unwrap_or(1.0) as f32;
    if ui.slider("s-mouse", c.row(), &mut ms, 0.1, 3.0, 0.05, "Mouse steering sensitivity (O)", &|v| if (v - 1.0).abs() < 0.01 { "OMSI".to_string() } else { format!("{:.0}%", v * 100.0) }) {
        s["mouse_sens"] = json!((ms * 100.0).round() / 100.0);
        *dirty = 0.3;
    }
    toggle_setting(ui, s, dirty, c.row(), "A right click ends the mouse steering (as in OMSI)", "mouse_right_off");
    toggle_setting(ui, s, dirty, c.row(), "Indicators cancel themselves (as the bus's script does)", "blinker_cancel");
    toggle_setting(ui, s, dirty, c.row(), "The keyboard brake stays on until the throttle (as in OMSI)", "brake_hold");
    toggle_setting(ui, s, dirty, c.row(), "Automatic clutch (manual gearboxes)", "auto_clutch");
    toggle_setting(ui, s, dirty, c.row(), "Hold manual gear buttons (release returns to neutral)", "momentary_gears");
    if ui.button("s-go-keys", c.row(), "Change the keys", Some("keyboard"), ButtonKind::Normal) {
        out.controls = Some(0);
    }
    let left = c.used();
    let mut c = Col::new(ui, cols[1], "Game controllers");
    // steering wheels: the wheel's own rotation and how much of it is the bus's full lock
    // (a real bus: about two and a half turns), force feedback the other way round
    let mut range = get(s, "wheel_range").as_f64().unwrap_or(900.0) as f32;
    if ui.slider("s-wrange", c.row(), &mut range, 180.0, 1800.0, 30.0, "Wheel rotation", &|v| format!("{v:.0}°")) {
        s["wheel_range"] = json!(range.round());
        *dirty = 0.3;
    }
    let mut lock = get(s, "wheel_lock").as_f64().unwrap_or(0.0) as f32;
    if ui.slider("s-wlock", c.row(), &mut lock, 0.0, 1800.0, 30.0, "Full lock at", &|v| if v < 45.0 { "OMSI".to_string() } else { format!("{v:.0}°") }) {
        s["wheel_lock"] = json!(if lock < 45.0 { 0.0 } else { lock.round() });
        *dirty = 0.3;
    }
    // the pedals' response: softer (below 1) or stronger (above 1) than the pedal reads
    for (key, label, id) in [("pedal_throttle", "Throttle pedal strength", "s-pedt"), ("pedal_brake", "Brake pedal strength", "s-pedb")] {
        let mut v = get(s, key).as_f64().unwrap_or(1.0) as f32;
        if ui.slider(id, c.row(), &mut v, 0.5, 2.0, 0.05, label, &|v| if (v - 1.0).abs() < 0.01 { "Normal".to_string() } else if v < 1.0 { format!("Softer x{v:.2}") } else { format!("Stronger x{v:.2}") }) {
            s[key] = json!((v * 100.0).round() / 100.0);
            *dirty = 0.3;
        }
    }
    toggle_setting(ui, s, dirty, c.row(), "Force feedback and vibration", "ff_enabled");
    toggle_setting(ui, s, dirty, c.row(), "Invert force feedback by default", "ff_invert");
    c.y += ui.paragraph("Wheels with a saved direction use their own setting under Controls → Game controllers.", Vec2::new(c.inner.x, c.y), c.inner.w, 12.5, Weight::Regular, TEXT_DIM) + 8.0;
    if ui.button("s-wreset", c.row(), "Reset wheel settings", Some("restart_alt"), ButtonKind::Normal) {
        s["wheel_range"] = json!(900.0);
        s["wheel_lock"] = json!(0.0);
        s["ff_invert"] = json!(false);
        s["ff_enabled"] = json!(true);
        *dirty = 0.3;
    }
    if ui.button("s-go-pads", c.row(), "Set up a wheel or pedals", Some("sports_esports"), ButtonKind::Normal) {
        out.controls = Some(1);
    }
    [left, c.used()]
}

/// What the driver sees: the seat, the views from it and around the bus, head tracking, VR.
fn camera_tab(ui: &mut Ui, s: &mut Value, dirty: &mut f32, out: &mut Outside, cols: [Rect; 2]) -> [f32; 2] {
    let mut c = Col::new(ui, cols[0], "Driver's view");
    // the driver's eye, moved from the bus's own camera
    for (key, label, id) in [("seat_y", "Seat forward / back", "s-seaty"), ("seat_z", "Seat up / down", "s-seatz"), ("seat_x", "Seat right / left", "s-seatx")] {
        let mut v = get(s, key).as_f64().unwrap_or(0.0) as f32;
        if ui.slider(id, c.row(), &mut v, -0.6, 0.6, 0.01, label, &|v| format!("{:+.0} cm", v * 100.0)) {
            s[key] = json!((v * 100.0).round() / 100.0);
            *dirty = 0.3;
        }
    }
    if ui.button("s-seatreset", c.row(), "Reset the seat position", Some("restart_alt"), ButtonKind::Normal) {
        for k in ["seat_x", "seat_y", "seat_z"] {
            s[k] = json!(0.0);
        }
        *dirty = 0.3;
    }
    let fov_key = if get(s, "triple_screen").as_bool().unwrap_or(false)
        && !get(s, "vr").as_bool().unwrap_or(false)
    {
        "triple_fov_deg"
    } else {
        "fov"
    };
    let mut fov = get(s, fov_key).as_f64().unwrap_or(0.0) as f32;
    if ui.slider("s-fov", c.row(), &mut fov, 0.0, 120.0, 1.0, "Field of view", &|v| if v < 20.0 { "Default".to_string() } else { format!("{v:.0}°") }) {
        s[fov_key] = json!(if fov < 20.0 { 0.0 } else { fov.round() });
        *dirty = 0.3;
    }
    let mut look = get(s, "look_sens").as_f64().unwrap_or(1.0) as f32;
    if ui.slider("s-look-sens", c.row(), &mut look, 0.1, 2.0, 0.05, "Mouse look sensitivity", &|v| if (v - 1.0).abs() < 0.01 { "OMSI".to_string() } else { format!("{:.0}%", v * 100.0) }) {
        s["look_sens"] = json!((look * 100.0).round() / 100.0);
        *dirty = 0.3;
    }
    c.section(ui, "A head at rest");
    let mut idle = get(s, "head_idle").as_f64().unwrap_or(0.0) as f32;
    if ui.slider("s-head-idle", c.row(), &mut idle, 0.0, 1.0, 0.05, "Head sway at a standstill", &|v| if v <= 0.0 { "Off".to_string() } else { format!("{:.0}%", v * 100.0) }) {
        s["head_idle"] = json!((idle * 100.0).round() / 100.0);
        *dirty = 0.3;
    }
    let mut pace = get(s, "head_idle_pace").as_f64().unwrap_or(1.0) as f32;
    if ui.slider("s-head-idle-pace", c.row(), &mut pace, 0.5, 2.0, 0.05, "Sway pace", &|v| format!("{:.0}%", v * 100.0)) {
        s["head_idle_pace"] = json!((pace * 100.0).round() / 100.0);
        *dirty = 0.3;
    }
    toggle_setting(ui, s, dirty, c.row(), "Driver's view turns with the steering", "steer_look");
    let mut angle = get(s, "steer_look_angle").as_f64().unwrap_or(30.0) as f32;
    if ui.slider("s-steer-look-angle", c.row(), &mut angle, 0.0, 60.0, 1.0, "Steering view angle", &|v| format!("{v:.0}°")) {
        s["steer_look_angle"] = json!(angle);
        *dirty = 0.3;
    }
    let mut response = get(s, "steer_look_response").as_f64().unwrap_or(0.25) as f32;
    if ui.slider("s-steer-look-response", c.row(), &mut response, 0.05, 1.0, 0.05, "Steering view response", &|v| format!("{:.0} ms", v * 1000.0)) {
        s["steer_look_response"] = json!(response);
        *dirty = 0.3;
    }
    toggle_setting(ui, s, dirty, c.row(), "Head moves with the bus", "head_movement");
    toggle_setting(ui, s, dirty, c.row(), "Camera glides between viewpoints", "driverview_smooth");
    toggle_setting(ui, s, dirty, c.row(), "Driver's hands in the cab view", "hands_in_cab");
    toggle_setting(ui, s, dirty, c.row(), "Right mouse button turns the view, Shift+right zooms (off: right zooms as in OMSI, the wheel button turns)", "alt_view");
    let left = c.used();
    let mut c = Col::new(ui, cols[1], "Outside views");
    toggle_setting(ui, s, dirty, c.row(), "Camera collisions (outside view)", "camera_collision");
    toggle_setting(ui, s, dirty, c.row(), "Driver at the wheel (outside views)", "driver");
    c.section(ui, "Head tracking");
    toggle_setting(ui, s, dirty, c.row(), "Head tracking (TrackIR and others through opentrack, UDP 4242)", "head_tracking");
    c.section(ui, "Triple screen");
    toggle_setting(
        ui,
        s,
        dirty,
        c.row(),
        "Three screen projections",
        "triple_screen",
    );
    toggle_setting(
        ui,
        s,
        dirty,
        c.row(),
        "Span three monitors at startup",
        "triple_span",
    );
    toggle_setting(
        ui,
        s,
        dirty,
        c.row(),
        "HUD on centre screen",
        "triple_hud_center",
    );
    if get(s, "triple_screen").as_bool().unwrap_or(false) {
        ui.label(
            c.row(),
            "Three equal screens in a horizontal row. OpenXR takes priority.",
        );
        let mut value = get(s, "triple_width_mm").as_f64().unwrap_or(600.0) as f32;
        if ui.slider(
            "s-triple-width_mm",
            c.row(),
            &mut value,
            200.0,
            2000.0,
            10.0,
            "Visible width of one panel",
            &|v| format!("{v:.0} mm"),
        ) {
            s["triple_width_mm"] = json!(value);
            *dirty = 0.3;
        }
        let mut value = get(s, "triple_distance_mm").as_f64().unwrap_or(650.0) as f32;
        if ui.slider(
            "s-triple-distance_mm",
            c.row(),
            &mut value,
            200.0,
            3000.0,
            10.0,
            "Eye to centre screen",
            &|v| format!("{v:.0} mm"),
        ) {
            s["triple_distance_mm"] = json!(value);
            s["triple_fov_deg"] = json!(0.0);
            *dirty = 0.3;
        }
        let mut value = get(s, "triple_bezel_mm").as_f64().unwrap_or(0.0) as f32;
        if ui.slider(
            "s-triple-bezel_mm",
            c.row(),
            &mut value,
            0.0,
            100.0,
            1.0,
            "Both frames at each join",
            &|v| format!("{v:.0} mm"),
        ) {
            s["triple_bezel_mm"] = json!(value);
            *dirty = 0.3;
        }
        let mut value = get(s, "triple_left_angle_deg").as_f64().unwrap_or(45.0) as f32;
        if ui.slider(
            "s-triple-left_angle_deg",
            c.row(),
            &mut value,
            0.0,
            90.0,
            1.0,
            "Left screen inward angle",
            &|v| format!("{v:.0}°"),
        ) {
            s["triple_left_angle_deg"] = json!(value);
            *dirty = 0.3;
        }
        let mut value = get(s, "triple_right_angle_deg").as_f64().unwrap_or(45.0) as f32;
        if ui.slider(
            "s-triple-right_angle_deg",
            c.row(),
            &mut value,
            0.0,
            90.0,
            1.0,
            "Right screen inward angle",
            &|v| format!("{v:.0}°"),
        ) {
            s["triple_right_angle_deg"] = json!(value);
            *dirty = 0.3;
        }
        let mut value = get(s, "triple_eye_height_mm").as_f64().unwrap_or(0.0) as f32;
        if ui.slider(
            "s-triple-eye_height_mm",
            c.row(),
            &mut value,
            -500.0,
            500.0,
            1.0,
            "Eye above screen centre",
            &|v| format!("{v:.0} mm"),
        ) {
            s["triple_eye_height_mm"] = json!(value);
            *dirty = 0.3;
        }
    }
    if cfg!(windows) {
        c.section(ui, "Virtual reality");
        toggle_setting(ui, s, dirty, c.row(), "Use OpenXR headset", "vr");
        if get(s, "vr").as_bool().unwrap_or(false) {
            sel_setting(ui, s, dirty, "s-vr-scale", c.row(), "Eye resolution", "vr_scale", &[("0.5", "50%"), ("0.65", "65%"), ("0.8", "80%"), ("1", "100%")]);
            sel_setting(ui, s, dirty, "s-vr-head-smoothing", c.row(), "Head tracking smoothing", "vr_head_smoothing_ms", &[("0", "Off"), ("5", "5 ms"), ("10", "10 ms"), ("20", "20 ms"), ("30", "30 ms")]);
            sel_setting(ui, s, dirty, "s-vr-mirror-rate", c.row(), "Bus mirror refresh", "vr_mirror_rate", &[("0", "Off"), ("8", "8/s"), ("16", "16/s"), ("24", "24/s"), ("32", "32/s"), ("48", "48/s"), ("60", "60/s"), ("90", "90/s"), ("120", "120/s"), ("180", "180/s"), ("240", "240/s"), ("360", "360/s"), ("-1", "Every frame")]);
            c.y += ui.paragraph("The rate is shared by all bus mirrors. Higher rates can reduce game FPS.", Vec2::new(c.inner.x, c.y), c.inner.w, 12.0, Weight::Regular, TEXT_DIM) + 8.0;
            toggle_setting(ui, s, dirty, c.row(), "Show headset picture on monitor", "vr_desktop_mirror");
            // (the VR keys head the Controls page's game list)
            if ui.button("s-go-vr-keys", c.row(), "Change the VR keys", Some("keyboard"), ButtonKind::Normal) {
                out.controls = Some(0);
            }
        }
    }
    [left, c.used()]
}

/// How loud the bus, the traffic and the surroundings are, and what passengers say.
fn sound_tab(ui: &mut Ui, s: &mut Value, dirty: &mut f32, cols: [Rect; 2]) -> [f32; 2] {
    let mut c = Col::new(ui, cols[0], "Volume");
    let mut vol = get(s, "volume").as_f64().unwrap_or(0.6) as f32;
    if ui.slider("s-vol", c.row(), &mut vol, 0.0, 1.0, 0.05, "Volume", &|v| format!("{:.0}%", v * 100.0)) {
        s["volume"] = json!((vol * 100.0).round() / 100.0);
        *dirty = 0.3;
    }
    for (key, label, id) in [("vol_ai", "Traffic", "s-volai"), ("vol_scenery", "Surroundings", "s-volsc")] {
        let mut v = get(s, key).as_f64().unwrap_or(1.0) as f32;
        if ui.slider(id, c.row(), &mut v, 0.0, 1.0, 0.05, label, &|v| format!("{:.0}%", v * 100.0)) {
            s[key] = json!((v * 100.0).round() / 100.0);
            *dirty = 0.3;
        }
    }
    toggle_setting(ui, s, dirty, c.row(), "Doppler effect", "doppler");
    sel_setting(ui, s, dirty, "s-voices", c.row(), "Passenger voices", "pax_voices", &[("all", "Greetings and tickets"), ("tickets", "Only the ticket asked for"), ("off", "Silent")]);
    [c.used(), 0.0]
}

/// How the world behaves: passengers, traffic, collisions, wear, the clock.
fn gameplay_tab(ui: &mut Ui, s: &mut Value, dirty: &mut f32, cols: [Rect; 2]) -> [f32; 2] {
    let mut c = Col::new(ui, cols[0], "Passengers");
    sel_setting(ui, s, dirty, "s-board", c.row(), "Boarding", "boarding", &[("auto", "Pay and take the ticket"), ("pay", "The driver sells the ticket"), ("walk", "Just walk in")]);
    toggle_setting(ui, s, dirty, c.row(), "Passengers pay the exact fare", "exact_fare");
    let mut pd = get(s, "pax_density").as_f64().unwrap_or(1.0) as f32;
    if ui.slider("s-pax", c.row(), &mut pd, 0.0, 2.0, 0.1, "How many passengers", &|v| format!("{:.0}%", v * 100.0)) {
        s["pax_density"] = json!((pd * 100.0).round() / 100.0);
        *dirty = 0.3;
    }
    toggle_setting(ui, s, dirty, c.row(), "Ability to get up (Ctrl+Shift+G)", "get_up");
    c.section(ui, "Traffic");
    sel_setting(ui, s, dirty, "s-unsched", c.row(), "Random traffic", "ai_unsched_factor", &[("25", "25%"), ("50", "50%"), ("75", "75%"), ("100", "100%"), ("150", "150%"), ("200", "200%")]);
    sel_setting(ui, s, dirty, "s-maxsched", c.row(), "Timetable vehicles", "ai_max_scheduled", &[("0", "All"), ("10", "At most 10"), ("25", "At most 25"), ("50", "At most 50")]);
    sel_setting(ui, s, dirty, "s-maxpark", c.row(), "Parked cars", "ai_max_parked", &[("-1", "None"), ("0", "Every space"), ("35", "At most 35"), ("100", "At most 100"), ("250", "At most 250")]);
    let left = c.used();
    // OMSI's own options (options.cfg)
    let mut c = Col::new(ui, cols[1], "Simulation");
    sel_setting(ui, s, dirty, "s-maint", c.row(), "Maintenance", "maintenance", &[("0", "Infinite (no wear)"), ("1", "Very bad"), ("2", "Bad"), ("3", "Normal"), ("4", "Good")]);
    toggle_setting(ui, s, dirty, c.row(), "Collisions with vehicles", "collision_vehicles");
    toggle_setting(ui, s, dirty, c.row(), "Collisions with objects (walls, poles)", "collision_objects");
    toggle_setting(ui, s, dirty, c.row(), "Collisions with people", "collision_pedestrians");
    toggle_setting(ui, s, dirty, c.row(), "Start at the real time", "use_real_time");
    toggle_setting(ui, s, dirty, c.row(), "Start on today's date", "use_real_date");
    // the game's clock follows this device's (the host's in multiplayer); the time cannot be set
    toggle_setting(ui, s, dirty, c.row(), "Sync the clock with the real time (locks the time)", "time_sync");
    // the weather follows the METAR report of the airport nearest the map; it cannot be changed then
    toggle_setting(ui, s, dirty, c.row(), "Sync the weather with METAR (locks the weather)", "metar_sync");
    // (in multiplayer the host's or the server's speed counts)
    sel_setting(ui, s, dirty, "s-timespeed", c.row(), "Time speed (not in multiplayer or with the real-time sync)", "time_speed", &[("1", "Real time"), ("2", "x2"), ("4", "x4"), ("8", "x8"), ("15", "x15"), ("30", "x30")]);
    [left, c.used()]
}

/// The language, what the screen shows besides the bus, online, the navigator, updates.
fn general_tab(ui: &mut Ui, s: &mut Value, dirty: &mut f32, out: &mut Outside, cols: [Rect; 2]) -> [f32; 2] {
    let mut c = Col::new(ui, cols[0], "Interface & online");
    {
        let langs: Vec<(&str, &str)> = core::LANGUAGES.iter().map(|l| (l.0, l.1)).collect();
        sel_setting(ui, s, dirty, "s-lang", c.row(), "Language", "language", &langs);
    }
    // (the launcher speaks the chosen language at once)
    crate::ui_language(get(s, "language").as_str().unwrap_or("ENG"));
    // (texts nobody has translated: translated on this machine, see `mt`)
    let was = get(s, "machine_translation").as_bool().unwrap_or(false);
    toggle_setting(ui, s, dirty, c.row(), "Translate the remaining texts automatically (offline, downloads 620 MB once)", "machine_translation");
    let now = get(s, "machine_translation").as_bool().unwrap_or(false);
    if now != was {
        crate::mt::enable(now);
    }
    let st = crate::mt::status();
    if now && !st.is_empty() && st != "Ready" {
        ui.text_in(&st, Rect::new(c.inner.x + 12.0, c.y - 6.0, c.inner.w - 24.0, 16.0), 11.5, omsi_ui::Weight::Regular, TEXT_FAINT, omsi_ui::paint::Align::Left);
        c.y += 14.0;
    }
    toggle_setting(ui, s, dirty, c.row(), "The launcher rests while a game runs (gives the graphics card to the game)", "launcher_rest");
    toggle_setting(ui, s, dirty, c.row(), "Discord Rich Presence", "discord_status");
    let help_height = ui.paragraph(
        "Shows the launcher or your map, bus, line and multiplayer status in Discord.",
        Vec2::new(c.inner.x + 12.0, c.y - 5.0),
        c.inner.w - 24.0,
        11.5,
        omsi_ui::Weight::Regular,
        TEXT_FAINT,
    );
    c.y += help_height + 3.0;
    toggle_setting(ui, s, dirty, c.row(), "Voice chat through GreenTeaSpeak (multiplayer)", "voice_chat");
    let help_height = ui.paragraph(
        "Players near you are heard from where they stand, when GreenTeaSpeak runs with the openOMSI plugin and the server names a voice server.",
        Vec2::new(c.inner.x + 12.0, c.y - 5.0),
        c.inner.w - 24.0,
        11.5,
        omsi_ui::Weight::Regular,
        TEXT_FAINT,
    );
    c.y += help_height + 3.0;
    // (the texts over the picture, the menu, the timetable and the navigator: larger for
    // those who find them hard to read, smaller for more of the picture; on a window taller
    // than 1080p they grow with it as well, and the launcher grows with its window anyway)
    let mut size = get(s, "ui_scale").as_f64().unwrap_or(1.0) as f32;
    if ui.slider("s-uiscale", c.row(), &mut size, 0.5, 2.0, 0.05, "Game interface size", &|v| format!("{:.0}%", v * 100.0)) {
        s["ui_scale"] = json!((size * 100.0).round() / 100.0);
        *dirty = 0.3;
    }
    toggle_setting(ui, s, dirty, c.row(), "Interface grows with the window", "ui_scale_window");
    // (the backgrounds of the whole interface - the navigator, the menu, the timetable, the
    // notes' plates - the texts staying solid; 85 % as designed)
    let mut op = get(s, "ui_opacity").as_f64().unwrap_or(0.85) as f32;
    if ui.slider("s-uiop", c.row(), &mut op, 0.2, 1.0, 0.05, "Interface opacity", &|v| format!("{:.0}%", v * 100.0)) {
        s["ui_opacity"] = json!((op * 100.0).round() / 100.0);
        *dirty = 0.3;
    }
    toggle_setting(ui, s, dirty, c.row(), "Name of the button under the mouse", "tooltips");
    toggle_setting(ui, s, dirty, c.row(), "Frame rate in the corner", "show_fps");
    toggle_setting(ui, s, dirty, c.row(), "Notes in the top-left corner", "notes");
    toggle_setting(ui, s, dirty, c.row(), "Chat in online games", "chat");
    toggle_setting(ui, s, dirty, c.row(), "Other players' names above their buses", "name_tags");
    c.section(ui, "Navigator");
    toggle_setting(ui, s, dirty, c.row(), "Navigator (Shift+N: map, schedule, off)", "navigator");
    toggle_setting(ui, s, dirty, c.row(), "Route arrows (as in OMSI 2)", "nav_arrows");
    toggle_setting(ui, s, dirty, c.row(), "AI vehicles on the map", "nav_ai");
    // the corner: a little screen with four corners to click
    let r = Rect::new(c.inner.x, c.y, c.inner.w, 70.0);
    ui.label(Rect::new(r.x, r.y, r.w * 0.45, 24.0), "Corner");
    let screen = Rect::new(r.x + r.w * 0.45, r.y, 110.0, 64.0);
    ui.p().rounded(screen, 6.0, Color::WHITE.alpha(0.05));
    ui.p().rounded_border(screen, 6.0, 1.0, Color::WHITE.alpha(0.12));
    let cur = get(s, "navigator_corner").as_str().unwrap_or("bottom-left").to_string();
    for (name, x, yy) in [("top-left", 0.0, 0.0), ("top-right", 1.0, 0.0), ("bottom-left", 0.0, 1.0), ("bottom-right", 1.0, 1.0)] {
        let cell = Rect::new(screen.x + 5.0 + x * (screen.w * 0.5), screen.y + 5.0 + yy * (screen.h * 0.5), screen.w * 0.5 - 10.0, screen.h * 0.5 - 10.0);
        let (h, _, clicked) = ui.interact(id_of(&format!("corner-{name}")), cell);
        if clicked {
            s["navigator_corner"] = json!(name);
            *dirty = 0.3;
        }
        let on = cur == name;
        ui.p().rounded(cell, 3.0, if on { ACCENT } else { Color::WHITE.alpha(if h { 0.2 } else { 0.08 }) });
    }
    // (dragged somewhere else in the game, #940: that place, until a corner is chosen)
    if let Some(a) = crate::navigator::placed_at(&cur) {
        let (cw, ch) = (screen.w * 0.5 - 10.0, screen.h * 0.5 - 10.0);
        let cell = Rect::new(screen.x + 5.0 + a[0] * (screen.w - 10.0 - cw), screen.y + 5.0 + a[1] * (screen.h - 10.0 - ch), cw, ch);
        ui.p().rounded(cell, 3.0, ACCENT);
    }
    c.y += 74.0;
    let left = c.used();
    // updates from the GitHub releases (see `crate::updater`)
    let mut c = Col::new(ui, cols[1], "Updates");
    toggle_setting(ui, s, dirty, c.row(), "Look for updates when the launcher starts", "update_check");
    toggle_setting(ui, s, dirty, c.row(), "Install updates without asking", "update_auto");
    {
        use crate::updater::Status;
        let r = c.row();
        let busy = matches!(out.update, Status::Checking | Status::Downloading { .. } | Status::Installing(_) | Status::WaitingForInstaller(_) | Status::Restarting(_));
        if ui.button("s-upd-check", Rect::new(r.x, r.y, 150.0, r.h), if busy { "Checking…" } else { "Check now" }, Some("refresh"), ButtonKind::Normal) && !busy {
            out.check_updates = true;
        }
        let text = match &out.update {
            Status::UpToDate if crate::updater::is_test_build(crate::updater::current_version()) => format!("{} is a test build: it is not updated", crate::updater::current_version()),
            Status::UpToDate => format!("{} is the latest version", crate::updater::current_version()),
            Status::Available(rel) => format!("{} is available", rel.version),
            Status::Failed(_) => "The last check failed".to_string(),
            _ => format!("This is openOMSI {}", crate::updater::current_version()),
        };
        ui.text_in(&text, Rect::new(r.x + 162.0, r.y, r.w - 162.0, r.h), 12.5, omsi_ui::Weight::Regular, TEXT_DIM, omsi_ui::paint::Align::Left);
    }
    if ui.button("s-upd-github", c.row(), "github.com/openOmsi-project/openOMSI", Some("open_in_new"), ButtonKind::Ghost) {
        crate::updater::open_url(crate::updater::REPO_URL);
    }
    // every setting at once: here at the end, not first on the page where it was the
    // control one saw before any other
    c.section(ui, "Reset");
    if ui.button("s-reset", c.row(), "Reset all settings...", Some("restart_alt"), ButtonKind::Danger) {
        out.reset = true;
    }
    [left, c.used()]
}

fn mb(v: i64) -> String {
    if v >= 1000 {
        format!("{:.1} GB", v as f64 / 1000.0)
    } else {
        format!("{v} MB")
    }
}

// --- controls ---------------------------------------------------------------------------------

fn action_text(names: &crate::describe::ControlNames, a: &str) -> String {
    known_action(a).unwrap_or_else(|| names.control(a))
}

fn control_names(l: &Launcher) -> &'static crate::describe::ControlNames {
    crate::describe::names(std::path::Path::new(&l.state.config.root), l.state.settings.get("language").and_then(|x| x.as_str()).unwrap_or("ENG"))
}

fn known_action(a: &str) -> Option<String> {
    if let Some(gear) = a.strip_prefix("kw_s_").and_then(|s| s.strip_suffix("_fest")) {
        return Some(format!("Gear {gear} (H-pattern)"));
    }
    let known: &[(&str, &str)] = &[
        ("throttle", "Throttle"),
        ("brake", "Brake"),
        ("throttle_amplify", "Throttle (full, kickdown)"),
        ("clutch", "Clutch"),
        ("steering_left", "Steer left"),
        ("steering_right", "Steer right"),
        ("steering_neutral", "Steering to centre"),
        ("parking_brake_toggle", "Parking brake"),
        ("blinker_left_set", "Indicator left"),
        ("blinker_right_set", "Indicator right"),
        ("blinker_left_toggle", "Indicator left (toggle)"),
        ("blinker_right_toggle", "Indicator right (toggle)"),
        ("blinker_off", "Indicators off"),
        ("blinker_warn_toggle", "Hazard lights"),
        ("gear_up", "Gear up (manual gearbox)"),
        ("gear_down", "Gear down (manual gearbox)"),
        ("horn", "Horn"),
        ("kw_scheinwerfer_toggle", "Headlights"),
        ("kw_standlicht_toggle", "Sidelights"),
        ("kw_fernlicht_toggle", "High beam"),
        ("kw_m_enginestart", "Starter"),
        ("kw_wipermode_up", "Wipers (next mode)"),
        ("cp_batterietrennschalter_toggle", "Battery / ignition"),
        ("automatic_D", "Gear D"),
        ("automatic_N", "Gear N"),
        ("automatic_R", "Gear R"),
        ("bus_doorfront0", "Front door (leaf 1)"),
        ("bus_doorfront1", "Front door (leaf 2)"),
        ("bus_dooraft", "Release rear doors"),
        ("door_1", "Door 1 (front), any bus"),
        ("door_2", "Door 2, any bus"),
        ("door_3", "Door 3, any bus"),
        ("door_4", "Door 4, any bus"),
        ("doors_all", "All doors, any bus"),
        ("ticket_give", "Sell the requested ticket"),
        ("view_set_driver", "Driver's view"),
        ("view_set_passenger", "Passenger view"),
        ("view_set_outside", "Outside view"),
        ("view_toggle_viewpoint", "Next view"),
        ("view_toggle_interior", "Cabin and outside, one key"),
        ("vr_recenter", "VR: Reset view"),
        ("vr_toggle_desktop_mirror", "VR: Monitor preview"),
        ("vr_toggle_mode", "VR: Switch VR / desktop"),
        ("vr_toggle_navigator", "VR: Toggle navigator"),
        ("vr_position_navigator", "VR: Position navigator"),
        ("exit", "Quit"),
        ("chat_open", "Multiplayer: write in the chat"),
        ("chat_toggle", "Multiplayer: show / hide the chat"),
        ("sim_pause", "Pause"),
        ("screenshot", "Screenshot"),
        ("quicksave", "Quicksave"),
        ("toggel_mouse_ctrl", "Toggle mouse steering"),
        ("toggel_ctrler", "Toggle game controllers"),
    ];
    known.iter().find(|k| k.0 == a).map(|k| k.1.to_string())
}

pub fn controls(l: &mut Launcher, area: Rect) {
    let body = l.page_title(area, "Controls", if l.pages.controls_tab == 0 { "Click a key and press the new one (hold Shift, Ctrl or Alt for a combination); Escape leaves it as it is." } else { "What each axis and button of a wheel, pedals or joystick does - OMSI 2's gamectrler.cfg, kept in the content folder." });
    let mut tab = l.pages.controls_tab;
    if l.ui.segmented("controls-tab", Rect::new(body.right() - 320.0, body.y - 46.0, 320.0, 34.0), &mut tab, &["Keyboard", "Game controllers"]) {
        l.pages.controls_tab = tab;
    }
    if l.pages.controls_tab == 1 {
        game_controllers(l, body);
        return;
    }
    l.pages.pads.cancel_feedback_test();
    // a key pressed while one binding waits for it
    if let (Some((sec, idx)), Some(code)) = (l.pages.capturing, l.ui.input.raw_key) {
        use winit::keyboard::KeyCode as K;
        if code == K::Escape {
            l.pages.capturing = None;
        } else if !matches!(code, K::ShiftLeft | K::ShiftRight | K::ControlLeft | K::ControlRight | K::AltLeft | K::AltRight | K::SuperLeft | K::SuperRight) {
            match crate::keys::dik_code(code) {
                Some(scan) => {
                    let m = omsi_content::input::chord(l.ui.input.shift, l.ui.input.ctrl, l.ui.input.alt) as i64;
                    let section = ["vehicles", "game"][sec];
                    let vr_binding = l.state.keybindings.get(section).and_then(|a| a.as_array())
                        .and_then(|a| a.get(idx)).and_then(|b| b.get("action"))
                        .and_then(|a| a.as_str()).is_some_and(|a| a.starts_with("vr_"));
                    if let Some(b) = l.state.keybindings.get_mut(section).and_then(|a| a.as_array_mut()).and_then(|a| a.get_mut(idx)) {
                        // (the entry's "held" bit is the action's, not the key's: it stays)
                        let hold = b.get("modifier").and_then(|x| x.as_i64()).unwrap_or(0) & omsi_content::input::KEY_HOLD as i64;
                        b["scan_code"] = json!(scan);
                        b["modifier"] = json!(m | hold);
                    }
                    save_keys(l, vr_binding);
                }
                None => l.state.set_status(format!("{code:?} has no DirectInput scan code the game understands."), true),
            }
            l.pages.capturing = None;
        }
        l.ui.input.raw_key = None;
    }
    // The keys below are the ones the game uses only with "Custom controls" (Settings →
    // Driving keys); the ready-made layouts keep W A S D / the arrows for driving. Say so,
    // with the switch right here - and changing a key switches by itself (see `save_keys`).
    let preset = l.state.settings.get("drive_keys").and_then(|v| v.as_str()).unwrap_or("simple").to_string();
    let body = if preset != "omsi" {
        let name = match preset.as_str() {
            "wasd" => "W A S D only",
            "arrows" => "Arrow keys only",
            _ => "W A S D + arrows",
        };
        let bw = if body.w < 700.0 { 150.0 } else { 200.0 };
        let text = format!("Driving keys: {name} (Settings). Those keys drive the bus and win over the list below. Change any key here and your own layout (Custom controls) is used from then on.");
        let tw = body.w - bw - 70.0;
        let th = l.ui.paragraph_height(&text, tw, 12.5, Weight::Medium);
        let bar = Rect::new(body.x, body.y, body.w, (th + 22.0).max(54.0));
        l.ui.p().rounded(bar, 8.0, ACCENT.alpha(0.1));
        l.ui.p().rounded_border(bar, 8.0, 1.0, ACCENT.alpha(0.45));
        l.ui.icon("info", Vec2::new(bar.x + 22.0, bar.center().y), 20.0, ACCENT);
        l.ui.paragraph(&text, Vec2::new(bar.x + 42.0, bar.center().y - th * 0.5), tw, 12.5, Weight::Medium, TEXT_SOFT);
        if l.ui.button("kb-use-custom", Rect::new(bar.right() - bw - 10.0, bar.center().y - 18.0, bw, 36.0), "Use these keys", Some("keyboard"), ButtonKind::Primary) {
            use_custom_keys(l);
        }
        let used = bar.h + 12.0;
        Rect::new(body.x, body.y + used, body.w, body.h - used)
    } else {
        body
    };
    let half = (body.w - GAP * 2.0) * 0.5;
    let names = control_names(l);
    for (sec, (title, sub, key)) in [("Driving & the bus", "The bus's own keys", "vehicles"), ("The game", "Menus, views, pausing", "game")].iter().enumerate() {
        let r = Rect::new(body.x + sec as f32 * (half + GAP * 2.0), body.y, half, body.h);
        l.ui.panel(r);
        let inner = l.ui.heading(Rect::new(r.x + 18.0, r.y + 14.0, r.w - 36.0, r.h - 28.0), title, Some(if sec == 0 { "directions_bus" } else { "sports_esports" }));
        l.ui.text_in(sub, Rect::new(inner.x, inner.y - 6.0, inner.w, 18.0), 12.0, Weight::Regular, TEXT_DIM, Align::Left);
        let mut filter = std::mem::take(&mut l.pages.kb_filter[sec]);
        l.ui.text_input(&format!("kb-filter-{sec}"), Rect::new(inner.x, inner.y + 18.0, inner.w, 34.0), &mut filter, "Filter…", Some("search"));
        l.pages.kb_filter[sec] = filter.clone();
        let q = filter.to_lowercase();
        let list: Vec<(usize, String, i64, i64)> = l.state.keybindings.get(*key).and_then(|a| a.as_array()).map(|a| a.iter().enumerate().map(|(i, b)| (i, b.get("action").and_then(|x| x.as_str()).unwrap_or("").to_string(), b.get("scan_code").and_then(|x| x.as_i64()).unwrap_or(0), b.get("modifier").and_then(|x| x.as_i64()).unwrap_or(0))).collect()).unwrap_or_default();
        let mut shown: Vec<(usize, String, String, bool)> = list
            .iter()
            .filter(|(_, a, s, m)| q.is_empty() || action_text(names, a).to_lowercase().contains(&q) || a.to_lowercase().contains(&q) || crate::keys::key_name(*s, *m).to_lowercase().contains(&q))
            .map(|(i, a, s, m)| {
                let clash = *s != 0 && list.iter().any(|(j, _, s2, m2)| j != i && s2 == s && m2 == m);
                (*i, action_text(names, a), crate::keys::key_name(*s, *m), clash)
            })
            .collect();
        if sec == 1 {
            shown.sort_by_key(|(_, label, _, _)| !label.starts_with("VR:"));
        }
        let capturing = l.pages.capturing;
        // what the row's buttons asked: (entry, cleared) a key cleared or to be pressed,
        // `more` another key for an entry's action (#854)
        let mut clicked: Option<(usize, bool)> = None;
        let mut more: Option<usize> = None;
        let time = l.ui.time;
        // a name the list does not have (a bus's own trigger a mod's readme gives a key, the
        // Urbanway's `ASS_toggle`): added to the list as a key of its own, as an [entry]
        // added to OMSI's keyboard.cfg by hand is (#854)
        let new_action = filter.trim();
        let mut list_top = inner.y + 62.0;
        if shown.is_empty() && new_action.len() > 1 && new_action.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            if l.ui.button(&format!("kb-add-{sec}"), Rect::new(inner.x, list_top, inner.w, 36.0), &format!("Add \"{new_action}\" and give it a key"), Some("add"), ButtonKind::Normal) {
                if let Some(a) = l.state.keybindings.get_mut(*key).and_then(|a| a.as_array_mut()) {
                    a.push(json!({ "action": new_action, "scan_code": 0, "modifier": 0 }));
                    l.pages.capturing = Some((sec, a.len() - 1));
                }
                // (the filter stays: the new row is the one it shows, waiting for its key)
            }
            list_top += 44.0;
        }
        l.ui.scroll_area(&format!("kb-{sec}"), Rect::new(inner.x - 6.0, list_top, inner.w + 12.0, inner.bottom() - list_top), &mut |ui, v| {
            let rh = 40.0;
            for (row, (i, label, keyn, clash)) in shown.iter().enumerate() {
                let rr = Rect::new(v.x + 6.0, v.y + row as f32 * rh, v.w - 16.0, rh - 4.0);
                if rr.bottom() < v.y - 50.0 {
                    continue;
                }
                ui.p().rounded(rr, 8.0, Color::WHITE.alpha(0.03));
                ui.text_in(label, Rect::new(rr.x + 12.0, rr.y, rr.w - 240.0, rr.h), 13.0, Weight::Medium, TEXT_SOFT, Align::Left);
                // another key for the same action (OMSI's file may give one action
                // several [entry]s; several actions on one key need nothing more than the
                // same key pressed for each)
                let pr = Rect::new(rr.right() - 222.0, rr.y + 5.0, 26.0, rr.h - 10.0);
                let (hp, _, cp) = ui.interact(id_of(&format!("kb-{sec}-{i}-more")), pr);
                ui.icon("add", pr.center(), 16.0, if hp { ACCENT } else { TEXT_FAINT });
                if cp {
                    more = Some(*i);
                }
                let kr = Rect::new(rr.right() - 190.0, rr.y + 5.0, 150.0, rr.h - 10.0);
                let waiting = capturing == Some((sec, *i));
                let id = id_of(&format!("kb-{sec}-{i}"));
                let (h, _, c) = ui.interact(id, kr);
                if c {
                    clicked = Some((*i, false));
                }
                let base = if waiting { ACCENT.alpha(0.25 + 0.15 * (time * 6.0).sin().abs()) } else if *clash { DANGER.alpha(0.22) } else { Color::WHITE.alpha(if h { 0.12 } else { 0.07 }) };
                ui.p().rounded(kr, 6.0, base);
                ui.p().rounded_border(kr, 6.0, 1.0, if waiting { ACCENT } else if *clash { DANGER } else { Color::WHITE.alpha(0.1) });
                ui.text_in(if waiting { "press a key…" } else { keyn }, kr, 12.0, Weight::Bold, if *clash { DANGER.lighten(0.3) } else { TEXT }, Align::Center);
                let xr = Rect::new(rr.right() - 32.0, rr.y + 5.0, 26.0, rr.h - 10.0);
                let (hx, _, cx) = ui.interact(id ^ 1, xr);
                ui.icon("close", xr.center(), 16.0, if hx { DANGER } else { TEXT_FAINT });
                if cx {
                    clicked = Some((*i, true));
                }
            }
            shown.len() as f32 * rh
        });
        match clicked {
            Some((i, true)) => {
                let vr_binding = l.state.keybindings.get(*key).and_then(|a| a.as_array())
                    .and_then(|a| a.get(i)).and_then(|b| b.get("action"))
                    .and_then(|a| a.as_str()).is_some_and(|a| a.starts_with("vr_"));
                if let Some(b) = l.state.keybindings.get_mut(*key).and_then(|a| a.as_array_mut()).and_then(|a| a.get_mut(i)) {
                    b["scan_code"] = json!(0);
                    b["modifier"] = json!(0);
                }
                save_keys(l, vr_binding);
            }
            Some((i, false)) => l.pages.capturing = Some((sec, i)),
            None => {}
        }
        if let Some(i) = more {
            if let Some(a) = l.state.keybindings.get_mut(*key).and_then(|a| a.as_array_mut()) {
                if let Some(b) = a.get(i).cloned() {
                    // (the held bit is the action's: it goes with it)
                    let hold = b.get("modifier").and_then(|x| x.as_i64()).unwrap_or(0) & omsi_content::input::KEY_HOLD as i64;
                    a.insert(i + 1, json!({ "action": b.get("action").cloned().unwrap_or(json!("")), "scan_code": 0, "modifier": hold }));
                    l.pages.capturing = Some((sec, i + 1));
                }
            }
        }
    }
    if !l.state.keybindings_error.is_empty() {
        let e = l.state.keybindings_error.clone();
        l.ui.text_in(&e, Rect::new(body.x, body.bottom() + 4.0, body.w, 18.0), 12.0, Weight::Medium, DANGER, Align::Left);
    }
}

/// Hide empty slots beyond the physical buttons without changing the saved controller file.
fn shown_button_count(buttons: &[(String, String)], physical: usize, revealed: Option<usize>) -> usize {
    physical
        .max(buttons.iter().rposition(|(action, _)| !action.trim().is_empty()).map(|i| i + 1).unwrap_or(0))
        .max(revealed.map(|i| i + 1).unwrap_or(0))
}

/// The game controllers tab (see `PadsView`).
fn game_controllers(l: &mut Launcher, body: Rect) {
    use crate::controllers::{DeviceCfg, Func};
    let hwnd = l.window.as_deref().and_then(crate::controllers::window_handle);
    let names = control_names(l);
    let pv = &mut l.pages.pads;
    if pv.io.is_none() {
        pv.io = Some(crate::controllers::Devices::new(hwnd, false));
    }
    if pv.devices.is_none() {
        let root = std::path::PathBuf::from(&l.state.config.root);
        pv.devices = Some(crate::controllers::read_cfg(&root));
    }
    // what the devices do now (and a button pressed while one is awaited)
    let mut pressed: Vec<(String, usize)> = Vec::new();
    let mut connected: Vec<crate::controllers::Connected> = Vec::new();
    if let Some(io) = pv.io.as_mut() {
        for (name, n, down) in io.poll() {
            if down {
                pressed.push((name, n));
            }
        }
        connected = io.connected();
    }
    let devices = pv.devices.get_or_insert_with(Vec::new);
    // devices connected but not set up yet can be added
    let list_w = (body.w * 0.32).min(360.0);
    let left = Rect::new(body.x, body.y, list_w, body.h);
    let right = Rect::new(body.x + list_w + GAP * 2.0, body.y, body.w - list_w - GAP * 2.0, body.h);
    l.ui.panel(left);
    let inner = l.ui.heading(Rect::new(left.x + 18.0, left.y + 14.0, left.w - 36.0, left.h - 28.0), "Devices", Some("sports_esports"));
    let mut add: Option<String> = None;
    let mut sel = pv.selected;
    let list_r = Rect::new(inner.x - 6.0, inner.y, inner.w + 12.0, inner.h - 108.0);
    let offs: Vec<String> = l.state.settings.get("ctrl_off").and_then(|v| v.as_str()).unwrap_or("").split('|').map(str::to_string).filter(|s| !s.is_empty()).collect();
    {
        let ui = &mut l.ui;
        let devices = &*devices;
        let connected = &connected;
        ui.scroll_area("pad-list", list_r, &mut |ui, v| {
            let mut y = v.y;
            for (i, d) in devices.iter().enumerate() {
                let on = connected.iter().any(|c| crate::controllers::names_match(&d.name, &c.name));
                let switched_off = offs.iter().any(|o| o.eq_ignore_ascii_case(&d.name));
                let r = Rect::new(v.x + 6.0, y, v.w - 12.0, 44.0);
                if ui.row(&format!("pad-{i}"), r, sel == i) {
                    sel = i;
                }
                ui.text_in(&d.name, Rect::new(r.x + 12.0, r.y, r.w - 40.0, r.h), 13.0, Weight::Medium, if on { TEXT } else { TEXT_DIM }, Align::Left);
                if switched_off {
                    ui.text_in("off", Rect::new(r.right() - 40.0, r.y, 30.0, r.h), 11.5, Weight::Bold, TEXT_FAINT, Align::Right);
                } else {
                    ui.icon(if on { "check_circle" } else { "remove" }, Vec2::new(r.right() - 18.0, r.center().y), 16.0, if on { OK } else { TEXT_FAINT });
                }
                y += 48.0;
            }
            for c in connected.iter().filter(|c| !devices.iter().any(|d| crate::controllers::names_match(&d.name, &c.name))) {
                let r = Rect::new(v.x + 6.0, y, v.w - 12.0, 38.0);
                if ui.button(&format!("pad-add-{}", c.name), r, &format!("Set up {}", c.name), Some("add"), ButtonKind::Primary) {
                    add = Some(c.name.clone());
                }
                y += 44.0;
            }
            if devices.is_empty() && connected.is_empty() {
                y += 4.0 + ui.paragraph("No game controller is connected, and none is set up. Connect a wheel, pedals or a joystick; a gamepad works without setting up (left stick steers, the triggers are the pedals).", Vec2::new(v.x + 6.0, y + 4.0), v.w - 12.0, 13.0, Weight::Regular, TEXT_DIM);
            }
            y - v.y
        });
    }
    if sel != pv.selected {
        release_feedback(&mut pv.io, &mut pv.feedback_test);
        pv.selected = sel;
        pv.capturing = false;
        pv.revealed_button = None;
        pv.wizard = None;
    }
    if let Some(name) = add {
        devices.push(DeviceCfg { name, second: "0".into(), ..Default::default() });
        pv.selected = devices.len() - 1;
        pv.revealed_button = None;
        pv.dirty = true;
        // a new device starts with the assistant
        pv.wizard = Some(Wizard { step: 0, rest: [None; 8], at: Vec::new(), error: None, calibration: None, ff_choice: None, test_strength: crate::ffb_calibration::PULSE_FORCE });
    }
    // the dead zone (a setting of the game's)
    let dz_r = Rect::new(inner.x, inner.bottom() - 98.0, inner.w, 34.0);
    let mut dz = l.state.settings.get("ctrl_deadzone").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32;
    if l.ui.slider("pad-dz", dz_r, &mut dz, 0.0, 0.3, 0.01, "Dead zone", &|v| format!("{:.0} %", v * 100.0)) {
        l.state.settings["ctrl_deadzone"] = json!(dz);
        l.state.settings_dirty = 0.3;
    }
    let save_r = Rect::new(inner.x, inner.bottom() - 48.0, inner.w, 40.0);
    if l.ui.button("pad-save", save_r, if pv.dirty { "Save" } else { "Saved" }, Some("save"), if pv.dirty { ButtonKind::Primary } else { ButtonKind::Normal }) && pv.dirty {
        match save_gamectrler(devices) {
            Ok(p) => {
                pv.dirty = false;
                l.state.set_status(format!("Game controllers saved to {}", p.display()), false);
            }
            Err(e) => l.state.set_status(format!("Not saved: {e}"), true),
        }
    }
    // the device shown
    l.ui.panel(right);
    let Some(d) = devices.get_mut(pv.selected) else { return };
    let inner = l.ui.heading(Rect::new(right.x + 18.0, right.y + 14.0, right.w - 36.0, right.h - 28.0), &d.name.clone(), Some("tune"));
    let live_dev = connected.iter().find(|c| crate::controllers::names_match(&d.name, &c.name));
    let live: Vec<(usize, f32)> = live_dev.map(|c| c.axes.clone()).unwrap_or_default();
    // every button the device has gets its line (DirectInput says how many)
    if let Some(n) = live_dev.map(|c| c.buttons).filter(|n| *n > d.buttons.len()) {
        d.buttons.resize(n, (String::new(), "0".into()));
    }
    // the assistant, over the device's page
    if let Some(w) = pv.wizard.as_mut() {
        let done = if w.step == WIZARD_STEPS.len() {
            feedback_setup(&mut l.ui, inner, w, d, &live, live_dev, &mut pv.io, &mut pv.feedback_test, hwnd,
                           l.state.settings.get("ff_invert").and_then(|v| v.as_bool()).unwrap_or(false))
        } else {
            wizard(&mut l.ui, inner, w, d, &live, live_dev.is_some(), live_dev.is_some_and(|c| c.ff_capable && !c.gamepad))
        };
        match done {
            Some(true) => {
                release_feedback(&mut pv.io, &mut pv.feedback_test);
                pv.wizard = None;
                pv.dirty = true;
                l.state.set_status("Set up: press Save to keep it (the buttons can be given their keys below).", false);
            }
            Some(false) => {
                release_feedback(&mut pv.io, &mut pv.feedback_test);
                pv.wizard = None;
            }
            None => {}
        }
        return;
    }
    // this device on or off (a second listing of the same wheel, a device not to be used)
    {
        let offs: Vec<String> = l.state.settings.get("ctrl_off").and_then(|v| v.as_str()).unwrap_or("").split('|').map(str::to_string).filter(|s| !s.is_empty()).collect();
        let mut on = !offs.iter().any(|o| o.eq_ignore_ascii_case(&d.name));
        if l.ui.toggle("pad-on", Rect::new(inner.right() - 400.0, inner.y - 36.0, 170.0, 30.0), &mut on, "Use this device") {
            let mut offs: Vec<String> = offs.into_iter().filter(|o| !o.eq_ignore_ascii_case(&d.name)).collect();
            if !on {
                offs.push(d.name.clone());
            }
            l.state.settings["ctrl_off"] = json!(offs.join("|"));
            l.state.settings_dirty = 0.3;
        }
    }
    if l.ui.button("pad-wizard", Rect::new(inner.right() - 220.0, inner.y - 36.0, 220.0, 30.0), "Set up step by step", Some("touch_app"), ButtonKind::Normal) {
        pv.wizard = Some(Wizard { step: 0, rest: [None; 8], at: Vec::new(), error: None, calibration: None, ff_choice: None, test_strength: crate::ffb_calibration::PULSE_FORCE });
    }
    const AXES: [&str; 8] = ["X axis", "Y axis", "Z axis", "X rotation", "Y rotation", "Z rotation", "Slider 1", "Slider 2"];
    let funcs: Vec<String> = Func::LABELS.iter().map(|s| s.to_string()).collect();
    let mut actions: Vec<String> = vec!["<none>".into()];
    actions.extend(l.state.keybindings.get("vehicles").and_then(|a| a.as_array()).map(|a| a.iter().filter_map(|b| b.get("action").and_then(|x| x.as_str()).map(String::from)).collect::<Vec<_>>()).unwrap_or_default());
    // H-pattern shifters use OMSI's "_fest" actions: pressing the gate selects the gear,
    // releasing it fires "_fest_off", which lets the bus script return to neutral.
    for a in ["kw_s_R_fest", "kw_s_1_fest", "kw_s_2_fest", "kw_s_3_fest", "kw_s_4_fest", "kw_s_5_fest", "kw_s_6_fest", "kw_s_7_fest", "kw_s_8_fest", "kw_s_9_fest", "kw_s_10_fest"] {
        if !actions.iter().any(|x| x.eq_ignore_ascii_case(a)) {
            actions.push(a.to_string());
        }
    }
    // the game's own view actions (looking around while held, the cameras, the views)
    for a in ["doors_all", "door_4", "door_3", "door_2", "door_1", "gear_up", "gear_down", "view_look_left", "view_look_right", "view_look_up", "view_look_down", "view_reset_direction", "view_interiorcam_plus", "view_interiorcam_minus", "view_toggle_viewpoint", "view_toggle_interior", "view_set_driver", "view_set_passenger", "view_set_outside", "sim_pause", "screenshot", "quicksave", "toggel_mouse_ctrl", "toggel_ctrler"] {
        if !actions.iter().any(|x| x == a) {
            actions.insert(1, a.to_string());
        }
    }
    actions.dedup();
    let labels: Vec<String> = actions.iter().enumerate().map(|(i, a)| if i == 0 { a.clone() } else { action_text(names, a) }).collect();
    let mut dirty = false;
    let lit = pv.last_pressed.filter(|(_, t)| t.elapsed().as_secs_f32() < 4.0).map(|(b, _)| b);
    // Some OMSI configs contain hundreds of empty trailing slots (the G920 report had
    // 131 entries for 18 physical buttons). Keep them on disk, but do not fill the UI with them.
    let shown_buttons = shown_button_count(&d.buttons, live_dev.map(|c| c.buttons).unwrap_or(0), pv.revealed_button);
    // (the axes, then every button of the device: the list scrolls - it stopped at the ten
    // buttons that fitted)
    let list = Rect::new(inner.x - 6.0, inner.y, inner.w + 12.0, inner.h - 50.0);
    let mut buttons_start_y = 0.0;
    l.ui.scroll_area("pad-detail", list, &mut |ui, v| {
        let x0 = v.x + 6.0;
        let w = v.w - 16.0;
        let mut y = v.y;
        if live_dev.is_some_and(|c| c.ff_capable) || d.ff_invert.is_some() {
            let (mut steering_force, mut vibration) = d.ff_scale.unwrap_or((1.0, 1.0));
            if ui.slider("pad-ff-steering", Rect::new(x0, y, w, ROW), &mut steering_force, 0.0, 2.0, 0.05, "Steering force", &|v| format!("{:.0}%", v * 100.0)) {
                d.ff_scale = Some((steering_force, vibration));
                dirty = true;
            }
            y += ROW + 6.0;
            if ui.slider("pad-ff-vibration", Rect::new(x0, y, w, ROW), &mut vibration, 0.0, 2.0, 0.05, "Vibration", &|v| format!("{:.0}%", v * 100.0)) {
                d.ff_scale = Some((steering_force, vibration));
                dirty = true;
            }
            y += ROW + 20.0;
            if !live_dev.is_some_and(|c| c.gamepad) {
                let mut invert = d.ff_invert.unwrap_or_else(|| l.state.settings.get("ff_invert").and_then(|v| v.as_bool()).unwrap_or(false));
                if ui.toggle("pad-ff-invert", Rect::new(x0, y, w, ROW), &mut invert, "Invert force feedback") {
                    d.ff_invert = Some(invert);
                    dirty = true;
                }
                y += ROW + 20.0;
            }
        }
        let lab_w = if w < 520.0 { 84.0 } else { 110.0 };
        let inv_w = 110.0;
        let shp_w = 140.0;
        let sel_w = (w - lab_w - inv_w - shp_w - 60.0 - 4.0 * GAP).clamp(120.0, 200.0);
        let bar_w = (w - lab_w - sel_w - inv_w - shp_w - 4.0 * GAP).max(30.0);
        let shapes: Vec<String> = crate::controllers::AXIS_SHAPES.iter().map(|s| s.0.to_string()).collect();
        for a in 0..8 {
            let r = Rect::new(x0, y, w, ROW);
            ui.label(Rect::new(r.x, r.y, lab_w, r.h), AXES[a]);
            let bar = Rect::new(r.x + lab_w + GAP, r.y + 12.0, bar_w, r.h - 24.0);
            ui.p().rounded(bar, 4.0, Color::WHITE.alpha(0.06));
            if let Some((_, v)) = live.iter().find(|(k, _)| *k == a) {
                let x = bar.x + (v.clamp(-1.0, 1.0) + 1.0) * 0.5 * bar.w;
                ui.p().rounded(Rect::new(x - 2.0, bar.y - 3.0, 4.0, bar.h + 6.0), 2.0, ACCENT);
            }
            let mut sel = (Func::code(d.axes[a].map(|x| x.0)) + 1) as usize;
            if ui.select(&format!("pad-axis-{a}"), Rect::new(bar.right() + GAP, r.y, sel_w, r.h), &mut sel, &funcs) {
                let inv = d.axes[a].map(|x| x.1).unwrap_or(false);
                d.axes[a] = Func::from_code(sel as i32 - 1).map(|f| (f, inv));
                dirty = true;
            }
            let mut inv = d.axes[a].map(|x| x.1).unwrap_or(false);
            if d.axes[a].is_some() && ui.toggle(&format!("pad-inv-{a}"), Rect::new(bar.right() + GAP + sel_w + GAP, r.y, inv_w, r.h), &mut inv, "Reversed") {
                if let Some(x) = d.axes[a].as_mut() {
                    x.1 = inv;
                }
                dirty = true;
            }
            if d.axes[a].is_some() {
                // the characteristic: the curve bits of the flags, the range extension kept
                let curve = d.axis_flags[a] & (4 | 8 | 0x10);
                let mut shp = crate::controllers::AXIS_SHAPES.iter().position(|s| s.1 == curve).unwrap_or(0);
                if ui.select(&format!("pad-shape-{a}"), Rect::new(bar.right() + GAP + sel_w + GAP + inv_w + GAP, r.y, shp_w, r.h), &mut shp, &shapes) {
                    d.axis_flags[a] = (d.axis_flags[a] & !(4 | 8 | 0x10)) | crate::controllers::AXIS_SHAPES[shp].1;
                    dirty = true;
                }
            }
            y += ROW + 6.0;
        }
        // buttons: their key actions
        y += 10.0;
        ui.text_in("Buttons", Rect::new(x0, y, w, 20.0), 14.0, Weight::Bold, TEXT, Align::Left);
        y += 26.0;
        buttons_start_y = y - v.y;
        let cols = if w < 560.0 { 1usize } else { 2 };
        let cw = (w - GAP * (cols - 1) as f32) / cols as f32;
        let per_row = ROW + 4.0;
        let rows = shown_buttons.div_ceil(cols);
        let latching = &mut d.latching;
        for (b, (act, _)) in d.buttons.iter_mut().take(shown_buttons).enumerate() {
            let (col, row) = (b / rows.max(1), b % rows.max(1));
            let r = Rect::new(x0 + col as f32 * (cw + GAP), y + row as f32 * per_row, cw, ROW);
            let label = match b.checked_sub(crate::controllers::HAT_BUTTONS) {
                Some(h) => format!("Hat {} {}", h / 4 + 1, ["up", "right", "down", "left"][h % 4]),
                None => format!("Button {}", b + 1),
            };
            if lit == Some(b) {
                ui.p().rounded(Rect::new(r.x - 4.0, r.y - 2.0, r.w + 8.0, r.h + 4.0), 6.0, ACCENT.alpha(0.28));
            }
            ui.label(Rect::new(r.x, r.y, 90.0, r.h), &label);
            // (a latching switch - a turn signal lever, a lit hazard button - also switches
            // when it comes out)
            let latch_w = 104.0;
            let mut sel = actions.iter().position(|a| a.eq_ignore_ascii_case(act)).unwrap_or(0);
            if ui.select(&format!("pad-btn-{b}"), Rect::new(r.x + 90.0, r.y, r.w - 90.0 - latch_w - GAP, r.h), &mut sel, &labels) {
                *act = if sel == 0 { String::new() } else { actions[sel].clone() };
                dirty = true;
            }
            let mut latched = latching.contains(&b);
            if ui.toggle(&format!("pad-latch-{b}"), Rect::new(r.right() - latch_w, r.y, latch_w, r.h), &mut latched, "Latching") {
                latching.retain(|x| *x != b);
                if latched {
                    latching.push(b);
                    latching.sort_unstable();
                }
                dirty = true;
            }
        }
        y += rows as f32 * per_row;
        y - v.y + 8.0
    });
    if dirty {
        pv.dirty = true;
    }
    // a button pressed on the device: its line (added up to it)
    if let Some((name, n)) = pressed.into_iter().find(|(name, _)| crate::controllers::names_match(&d.name, name)) {
        if crate::controllers::names_match(&d.name, &name) && n < crate::controllers::HAT_BUTTONS + 16 {
            while d.buttons.len() <= n {
                d.buttons.push((String::new(), "0".into()));
                pv.dirty = true;
            }
            pv.capturing = false;
            pv.revealed_button = Some(n);
            let cols = if list.w - 16.0 < 560.0 { 1usize } else { 2 };
            let rows = shown_buttons.max(n + 1).div_ceil(cols).max(1);
            let row = n % rows;
            l.ui.scroll_to("pad-detail", buttons_start_y + row as f32 * (ROW + 4.0), ROW, list.h);
            pv.last_pressed = Some((n, std::time::Instant::now()));
            let now = d.buttons.get(n).map(|b| b.0.clone()).filter(|a| !a.is_empty());
            let label = match n.checked_sub(crate::controllers::HAT_BUTTONS) {
                Some(h) => format!("hat {} {}", h / 4 + 1, ["up", "right", "down", "left"][h % 4]),
                None => format!("button {}", n + 1),
            };
            l.state.set_status(match now {
                Some(a) => format!("{name}: {label} - {} (lit in the list: choose another there)", action_text(names, &a)),
                None => format!("{name}: {label} - nothing yet (lit in the list: choose what it does)"),
            }, false);
        }
    }
    let add_r = Rect::new(inner.x, inner.bottom() - 40.0, 260.0, 36.0);
    if l.ui.button("pad-add-button", add_r, if pv.capturing { "Press a button on the device…" } else { "Add a button" }, Some("add"), ButtonKind::Normal) {
        pv.capturing = !pv.capturing;
    }
}

/// The steps of the set-up assistant (see `Wizard`): what the player is asked each time.
const WIZARD_STEPS: [(&str, &str); 5] = [
    ("Let go of everything", "Take your hands off the wheel and your feet off the pedals (the wheel in the middle), then press Next."),
    ("Steering", "Turn the wheel (or move the stick) all the way to the LEFT and hold it there, then press Next."),
    ("Throttle", "Press the throttle pedal all the way down and hold it, then press Next. No pedals: Skip."),
    ("Brake", "Press the brake pedal all the way down and hold it, then press Next. No brake pedal: Skip."),
    ("Clutch", "Press the clutch pedal all the way down and hold it, then press Next. No clutch: Skip."),
];

/// One frame of the assistant in `r`; Some(true) when it has set the device up, Some(false)
/// when the player gave up.
fn wizard(ui: &mut Ui, r: Rect, w: &mut Wizard, d: &mut crate::controllers::DeviceCfg, live: &[(usize, f32)], connected: bool, feedback: bool) -> Option<bool> {
    let (title, text) = WIZARD_STEPS[w.step];
    ui.text_in(&format!("Step {} of {}: {title}", w.step + 1, WIZARD_STEPS.len()), Rect::new(r.x, r.y, r.w, 26.0), 17.0, Weight::Bold, TEXT, Align::Left);
    let mut y = r.y + 34.0;
    y += ui.paragraph(text, Vec2::new(r.x, y), r.w, 13.5, Weight::Regular, TEXT_SOFT) + 10.0;
    if !connected {
        y += ui.paragraph("The device is not connected: plug it in (the list on the left marks it green).", Vec2::new(r.x, y), r.w, 13.0, Weight::Medium, DANGER);
    }
    if let Some(e) = &w.error {
        y += ui.paragraph(e, Vec2::new(r.x, y), r.w, 13.0, Weight::Medium, DANGER) + 6.0;
    }
    // the axes as they stand, so that the player sees the device answer
    for (k, v) in live {
        let bar = Rect::new(r.x + 90.0, y + 8.0, (r.w - 100.0).max(40.0), 8.0);
        ui.text_in(["X", "Y", "Z", "Rx", "Ry", "Rz", "Slider 1", "Slider 2"][*k], Rect::new(r.x, y, 84.0, 24.0), 12.0, Weight::Medium, TEXT_DIM, Align::Left);
        ui.p().rounded(bar, 4.0, Color::WHITE.alpha(0.06));
        let x = bar.x + (v.clamp(-1.0, 1.0) + 1.0) * 0.5 * bar.w;
        ui.p().rounded(Rect::new(x - 2.0, bar.y - 4.0, 4.0, bar.h + 8.0), 2.0, ACCENT);
        y += 26.0;
    }
    let now = |live: &[(usize, f32)]| {
        let mut a = [None; 8];
        for (k, v) in live {
            a[*k] = Some(*v);
        }
        a
    };
    let by = r.bottom() - 40.0;
    if ui.button("wiz-cancel", Rect::new(r.x, by, 120.0, 36.0), "Cancel", None, ButtonKind::Ghost) {
        return Some(false);
    }
    let skip = w.step >= 2 && ui.button("wiz-skip", Rect::new(r.right() - 260.0, by, 110.0, 36.0), "Skip", None, ButtonKind::Normal);
    let next = ui.button("wiz-next", Rect::new(r.right() - 140.0, by, 140.0, 36.0), if w.step + 1 == WIZARD_STEPS.len() && !feedback { "Finish" } else { "Next" }, Some("chevron_right"), ButtonKind::Primary);
    if !(next || skip) {
        return None;
    }
    let cur = now(live);
    w.error = None;
    if w.step == 0 {
        if live.is_empty() {
            w.error = Some("The device shows no axis yet: move the wheel and the pedals a little, let go, and press Next again.".into());
            return None;
        }
        w.rest = cur;
    } else if skip {
        w.at.push([None; 8]);
    } else {
        // the axis that moved most since everything was let go (one taken before is not
        // taken again - but the throttle's axis may turn out to be the brake's too)
        let used: Vec<usize> = w.at.iter().filter_map(|a| moved_most(&w.rest, a, &[]).map(|m| m.0)).collect();
        let exclude: Vec<usize> = if w.step == 3 { used.iter().copied().take(1).collect() } else { used.clone() };
        match moved_most(&w.rest, &cur, &exclude) {
            Some(_) => w.at.push(cur),
            None => {
                w.error = Some("Nothing moved far enough. Hold it all the way, then press Next (or Skip).".into());
                return None;
            }
        }
    }
    w.step += 1;
    if w.step < WIZARD_STEPS.len() {
        return None;
    }
    let axes = wizard_result(&w.rest, &w.at);
    if feedback && axes.iter().any(|a| matches!(a, Some((crate::controllers::Func::Steering, _)))) {
        return None;
    }
    d.axes = axes;
    Some(true)
}

fn feedback_setup(
    ui: &mut Ui, r: Rect, w: &mut Wizard, d: &mut crate::controllers::DeviceCfg,
    live: &[(usize, f32)], device: Option<&crate::controllers::Connected>,
    io: &mut Option<crate::controllers::Devices>, active: &mut bool, hwnd: Option<isize>, global_invert: bool,
) -> Option<bool> {
    let axes = wizard_result(&w.rest, &w.at);
    let axis = axes.iter().position(|a| matches!(a, Some((crate::controllers::Func::Steering, _))));
    ui.text_in("Force feedback direction", Rect::new(r.x, r.y, r.w, 26.0), 17.0, Weight::Bold, TEXT, Align::Left);
    let warning = "INJURY RISK: TAKE YOUR HANDS OFF THE WHEEL. Keep hands and fingers clear before starting and throughout the test.";
    let warning_height = ui.paragraph_height(warning, r.w - 58.0, 14.5, Weight::Bold) + 20.0;
    let warning_rect = Rect::new(r.x, r.y + 34.0, r.w, warning_height);
    ui.p().rounded(warning_rect, 6.0, DANGER.alpha(0.15));
    ui.p().rounded_border(warning_rect, 6.0, 1.5, DANGER);
    ui.icon("warning", Vec2::new(warning_rect.x + 22.0, warning_rect.center().y), 26.0, DANGER);
    ui.paragraph(warning, Vec2::new(warning_rect.x + 44.0, warning_rect.y + 10.0), warning_rect.w - 58.0, 14.5, Weight::Bold, DANGER);
    let body_y = warning_rect.bottom() + 12.0;
    ui.scroll_area("wiz-ff-body", Rect::new(r.x, body_y, r.w, r.bottom() - 56.0 - body_y), &mut |ui, r| {
        let mut y = r.y;
        y += ui.paragraph("The test applies two short forces in opposite directions. Finish and press Save to keep the detected direction for this wheel.", Vec2::new(r.x, y), r.w, 13.5, Weight::Regular, TEXT_SOFT) + 16.0;
        if let Some((started, test)) = w.calibration.as_mut() {
            if *active {
                let position = axis.and_then(|a| live.iter().find(|(k, _)| *k == a).map(|(_, x)| *x));
                if let Some(force) = test.update(started.elapsed().as_secs_f32(), position) {
                    if !device.zip(axis).zip(io.as_mut()).is_some_and(|((device, axis), io)| io.calibration_pulse(&device.name, axis, force)) {
                        test.fail("Force feedback is unavailable. Choose the direction manually.");
                    }
                }
                if let Some(result) = test.result {
                    release_feedback(io, active);
                    if let Ok(invert) = result {
                        w.ff_choice = Some(invert);
                    }
                }
            }
            let (message, color) = match test.result {
                Some(Ok(false)) => ("Direction detected: normal", OK),
                Some(Ok(true)) => ("Direction detected: inverted", OK),
                Some(Err(message)) => (message, DANGER),
                None => ("Testing: keep your hands off the wheel…", TEXT_SOFT),
            };
            y += ui.paragraph(message, Vec2::new(r.x, y), r.w, 13.0, Weight::Medium, color) + 12.0;
        }
        if !*active {
            ui.slider("wiz-ff-strength", Rect::new(r.x, y, r.w, ROW), &mut w.test_strength,
                      crate::ffb_calibration::PULSE_FORCE, crate::ffb_calibration::MAX_PULSE_FORCE, 0.01,
                      "Test strength", &|v| format!("{:.0}%", v * 100.0));
            y += ROW + 8.0;
            y += ui.paragraph("If the wheel barely moves, increase Test strength and retry. Keep your hands clear.", Vec2::new(r.x, y), r.w, 13.0, Weight::Regular, TEXT_DIM) + 10.0;
            if ui.button("wiz-ff-test", Rect::new(r.x, y, 180.0, 36.0), "Start test", Some("play_arrow"), ButtonKind::Primary) {
                if device.is_none() || axis.is_none() {
                    w.error = Some("The wheel is unavailable. Reconnect it and try again.".into());
                } else {
                    *io = None;
                    *io = Some(crate::controllers::Devices::new(hwnd, true));
                    *active = true;
                    w.error = None;
                    log::info!("FFB calibration: device {}, raw steering axis {:?}, test strength {:.0}%", device.unwrap().name, axis, w.test_strength * 100.0);
                    w.calibration = Some((std::time::Instant::now(), crate::ffb_calibration::Calibration::new(w.test_strength)));
                }
            }
            y += 48.0;
            let mut invert = w.ff_choice.or(d.ff_invert).unwrap_or(global_invert);
            if ui.toggle("wiz-ff-manual", Rect::new(r.x, y, r.w, ROW), &mut invert, "Invert force feedback") {
                w.ff_choice = Some(invert);
            }
            y += ROW + 8.0;
            y += ui.paragraph("If detection is inconclusive, retry or choose the direction manually. You can change it later on this device's page.", Vec2::new(r.x, y), r.w, 13.0, Weight::Regular, TEXT_DIM) + 8.0;
        }
        if let Some(error) = &w.error {
            y += ui.paragraph(error, Vec2::new(r.x, y), r.w, 13.0, Weight::Medium, DANGER) + 8.0;
        }
        y - r.y
    });
    let by = r.bottom() - 40.0;
    if ui.button("wiz-cancel", Rect::new(r.x, by, 120.0, 36.0), "Cancel", None, ButtonKind::Ghost) {
        return Some(false);
    }
    if !*active && ui.button("wiz-ff-finish", Rect::new(r.right() - 140.0, by, 140.0, 36.0), "Finish", Some("check"), ButtonKind::Primary) {
        d.axes = axes;
        d.ff_invert = Some(w.ff_choice.or(d.ff_invert).unwrap_or(global_invert));
        return Some(true);
    }
    None
}

/// The axes the assistant found: `rest` where everything rested, `at` where the axes stood
/// with the wheel turned left, the throttle, the brake and the clutch pressed (all None: that
/// step skipped).
fn wizard_result(rest: &[Option<f32>; 8], at: &[[Option<f32>; 8]]) -> [Option<(crate::controllers::Func, bool)>; 8] {
    use crate::controllers::Func;
    let mut axes: [Option<(Func, bool)>; 8] = [None; 8];
    let steer = at.first().and_then(|a| moved_most(rest, a, &[]));
    if let Some((k, delta)) = steer {
        // turned left the value falls: else the axis runs the other way
        axes[k] = Some((Func::Steering, delta > 0.0));
    }
    let taken: Vec<usize> = steer.map(|s| vec![s.0]).unwrap_or_default();
    let pedal = |i: usize, ex: &[usize]| at.get(i).and_then(|a| moved_most(rest, a, ex));
    let throttle = pedal(1, &taken);
    let brake = pedal(2, &taken);
    match (throttle, brake) {
        // one axis for both (pedals on a single axis): the throttle towards the raw maximum
        // (as in Omsi.exe), else the axis is reversed
        (Some((kt, dt)), Some((kb, db))) if kt == kb && dt * db < 0.0 => axes[kt] = Some((Func::ThrottleBrake, dt < 0.0)),
        _ => {
            // a pedal pressed goes towards 1
            if let Some((k, dl)) = throttle {
                axes[k] = Some((Func::Throttle, dl < 0.0));
            }
            if let Some((k, dl)) = brake.filter(|b| Some(b.0) != throttle.map(|t| t.0)) {
                axes[k] = Some((Func::Brake, dl < 0.0));
            }
        }
    }
    let mut ex = taken.clone();
    ex.extend(throttle.map(|t| t.0));
    ex.extend(brake.map(|t| t.0));
    if let Some((k, dl)) = pedal(3, &ex) {
        axes[k] = Some((Func::Clutch, dl < 0.0));
    }
    axes
}

/// The axis that moved most from `rest` to `now` (at least a sixth of its travel), not one of
/// `exclude`: (slot, how far, signed).
fn moved_most(rest: &[Option<f32>; 8], now: &[Option<f32>; 8], exclude: &[usize]) -> Option<(usize, f32)> {
    (0..8)
        .filter(|k| !exclude.contains(k))
        .filter_map(|k| Some((k, now[k]? - rest[k].unwrap_or(0.0))))
        .filter(|(_, d)| d.abs() > 0.33)
        .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
}

/// Write the devices to the content folder's `Inputs/gamectrler.cfg` (OMSI 2's own is only
/// read; the game takes the content folder's first).
fn save_gamectrler(devices: &[crate::controllers::DeviceCfg]) -> Result<std::path::PathBuf, String> {
    let candidate = core::content_dir().unwrap_or_else(core::data_dir).join("Inputs");
    let dir = if (candidate.exists() || std::fs::create_dir_all(&candidate).is_ok()) && omsi_cfg::is_writable(&candidate) {
        candidate
    } else {
        core::data_dir().join("Inputs")
    };
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let p = dir.join("gamectrler.cfg");
    std::fs::write(&p, crate::controllers::cfg_text(devices)).map_err(|e| e.to_string())?;
    omsi_cfg::content_changed();
    Ok(p)
}

/// Settings → Driving keys: "Custom controls", the keys of the Controls page.
fn use_custom_keys(l: &mut Launcher) -> bool {
    if l.state.settings.get("drive_keys").and_then(|v| v.as_str()) == Some("omsi") {
        return false;
    }
    l.state.settings["drive_keys"] = json!("omsi");
    l.state.settings_dirty = 0.3;
    l.state.set_status("Driving keys: Custom controls - the game uses the keys of this page.", false);
    true
}

fn save_keys(l: &mut Launcher, vr_binding: bool) {
    match core::save_keybindings(&l.state.keybindings) {
        Ok(()) => {
            l.state.keybindings_error.clear();
            if let Ok(k) = core::get_keybindings() {
                l.state.keybindings = k;
            }
            // a key changed is a key the player wants to use: with a ready-made layout it
            // would be ignored wherever that layout has a key of its own
            if !vr_binding && use_custom_keys(l) {
                l.state.set_status("Key bindings saved; Driving keys switched to Custom controls so the game uses them.", false);
            } else {
                l.state.set_status("Key bindings saved.", false);
            }
        }
        Err(e) => {
            l.state.keybindings_error = format!("{e:#}");
            l.state.set_status(format!("{e:#}"), true);
        }
    }
}

// --- sessions ---------------------------------------------------------------------------------

fn ago(t: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(t);
    let s = now.saturating_sub(t);
    if s < 60 {
        format!("{s} s")
    } else if s < 3600 {
        format!("{} min", s / 60)
    } else {
        format!("{} h {} min", s / 3600, (s / 60) % 60)
    }
}

pub fn sessions(l: &mut Launcher, area: Rect) {
    let body = l.page_title(area, "Sessions", "The games you started, and who drives with you.");
    let list = l.state.instances.clone();
    if list.is_empty() {
        let r = Rect::new(body.x, body.y, body.w.min(720.0), 120.0);
        l.ui.panel(r);
        l.ui.icon("sports_esports", Vec2::new(r.x + 40.0, r.center().y), 36.0, TEXT_FAINT);
        l.ui.paragraph("No game is running. Start a duty on the Drive page; to drive with friends, turn on hosting on the Multiplayer page and give them the code shown here.", Vec2::new(r.x + 76.0, r.y + 30.0), r.w - 100.0, 13.5, Weight::Regular, TEXT_DIM);
        return;
    }
    let names: std::collections::HashMap<String, String> = l.state.vehicles.iter().map(|v| (v.file.clone(), v.name.clone())).collect();
    let short_bus = |b: &str| names.get(b).cloned().unwrap_or_else(|| b.rsplit('/').next().unwrap_or("").trim_end_matches(".bus").to_string());
    let mut y = body.y - l.ui.scroll.get(&id_of("sessions")).copied().unwrap_or(0.0);
    // (the page's whole width, as the other pages: at 900 a wide window had the cards in its
    // left half and the buttons in the middle of nowhere)
    let view = Rect::new(body.x, body.y, body.w, body.h);
    let mut actions: Vec<(u32, &str)> = Vec::new();
    let mut copy: Option<String> = None;
    l.ui.push_clip(view, 0.0);
    for i in &list {
        let lan = i.lan_status.clone().unwrap_or(Value::Null);
        let role = lan.get("role").and_then(|x| x.as_str()).unwrap_or("");
        let players: Vec<Value> = lan.get("players").and_then(|x| x.as_array()).cloned().unwrap_or_default();
        let chat: Vec<String> = lan.get("chat").and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|c| c.as_str().map(String::from)).collect()).unwrap_or_default();
        let warnings: Vec<String> = lan.get("warnings").and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|c| c.as_str().map(String::from)).collect()).unwrap_or_default();
        let log_open = l.state.open_logs.contains(&i.pid);
        let log_lines = l.state.logs.get(&i.pid).cloned().unwrap_or_default();
        let mut h = 96.0;
        if role == "host" {
            h += 96.0;
        } else if !role.is_empty() {
            h += 24.0;
        }
        h += if players.is_empty() { 0.0 } else { 26.0 + players.len() as f32 * 20.0 };
        h += if chat.is_empty() { 0.0 } else { 26.0 + chat.len().min(6) as f32 * 18.0 };
        h += warnings.len() as f32 * 20.0;
        if log_open {
            h += 220.0;
        }
        let r = Rect::new(view.x, y, view.w, h);
        l.ui.panel(r);
        let running = i.running;
        let c = Vec2::new(r.x + 24.0, r.y + 28.0);
        if running {
            let pulse = (l.ui.time * 3.0).sin() * 0.5 + 0.5;
            l.ui.p().circle(c, 6.0 + 3.0 * pulse, OK.alpha(0.25));
        }
        l.ui.p().circle(c, 6.0, if running { OK } else { TEXT_FAINT });
        let duty = i.line.as_ref().map(|ln| format!(" · line {ln}{}", i.tour.as_ref().map(|t| format!(" / {t}")).unwrap_or_default())).unwrap_or_default();
        l.ui.text_in(&format!("{} · {}{duty}", short_map(&i.map), short_bus(&i.bus)), Rect::new(r.x + 42.0, r.y + 16.0, r.w - 260.0, 24.0), 16.0, Weight::Black, TEXT, Align::Left);
        let status = if running {
            if l.state.stopping.contains(&i.pid) || i.stopping.is_some() { "stopping - saving the run…".to_string() } else { format!("running for {}", ago(i.started)) }
        } else {
            let how = if i.exit_code == Some(0) { String::new() } else if i.killed { " (killed - it did not end by itself, the run is not saved)".into() } else { i.exit_code.map(|c| format!(" (exit code {c})")).unwrap_or_default() };
            format!("ended{how}")
        };
        l.ui.text_in(&format!("{status} · driver {}", i.profile), Rect::new(r.x + 42.0, r.y + 42.0, r.w - 60.0, 18.0), 12.0, Weight::Regular, TEXT_DIM, Align::Left);
        l.ui.text_in(&i.last_line, Rect::new(r.x + 42.0, r.y + 62.0, r.w - 60.0, 18.0), 11.5, Weight::Regular, TEXT_FAINT, Align::Left);
        // buttons
        let bw = 110.0;
        if running {
            let stopping = l.state.stopping.contains(&i.pid);
            if l.ui.button(&format!("stop-{}", i.pid), Rect::new(r.right() - 18.0 - bw, r.y + 14.0, bw, 34.0), if stopping { "Stopping…" } else { "Stop" }, Some("close"), ButtonKind::Danger) && !stopping {
                actions.push((i.pid, "stop"));
            }
        }
        if l.ui.button(&format!("log-{}", i.pid), Rect::new(r.right() - 18.0 - bw - if running { bw + 8.0 } else { 0.0 }, r.y + 14.0, bw, 34.0), if log_open { "Hide log" } else { "Show log" }, Some("receipt_long"), ButtonKind::Normal) {
            actions.push((i.pid, "log"));
        }
        let mut yy = r.y + 86.0;
        if role == "host" {
            let code = lan.get("code").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let box_r = Rect::new(r.x + 20.0, yy, r.w - 40.0, 84.0);
            l.ui.p().rounded(box_r, 10.0, ACCENT.alpha(0.10));
            l.ui.p().rounded_border(box_r, 10.0, 1.0, ACCENT.alpha(0.5));
            l.ui.text_in("SESSION CODE", Rect::new(box_r.x + 16.0, box_r.y + 8.0, 200.0, 16.0), 10.5, Weight::Black, ACCENT, Align::Left);
            l.ui.text_in(&code, Rect::new(box_r.x + 16.0, box_r.y + 26.0, box_r.w - 170.0, 30.0), 20.0, Weight::Condensed, TEXT, Align::Left);
            l.ui.text_in("Your friends paste it into Multiplayer → Connect by Code.", Rect::new(box_r.x + 16.0, box_r.y + 58.0, box_r.w - 170.0, 18.0), 11.5, Weight::Regular, TEXT_DIM, Align::Left);
            if l.ui.button(&format!("copy-{}", i.pid), Rect::new(box_r.right() - 146.0, box_r.y + 24.0, 130.0, 36.0), "Copy code", Some("content_copy"), ButtonKind::Primary) {
                copy = Some(code.clone());
            }
            yy += 96.0;
        } else if role == "client" {
            let connected = lan.get("connected").and_then(|x| x.as_bool()).unwrap_or(false);
            let text = if let Some(rej) = lan.get("rejected").and_then(|x| x.as_str()) {
                format!("not connected: {rej}")
            } else if connected {
                format!("connected to {}", lan.get("host_name").and_then(|x| x.as_str()).unwrap_or(""))
            } else {
                "connecting…".to_string()
            };
            l.ui.text_in(&format!("Multiplayer: {text}"), Rect::new(r.x + 42.0, yy, r.w - 60.0, 20.0), 12.5, Weight::Medium, if connected { OK } else { WARN }, Align::Left);
            yy += 24.0;
        }
        if !players.is_empty() {
            l.ui.text_in("PLAYERS", Rect::new(r.x + 42.0, yy, 200.0, 18.0), 10.5, Weight::Black, TEXT_FAINT, Align::Left);
            yy += 22.0;
            for p in &players {
                let s = |k: &str| p.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
                let pax = p.get("passengers").and_then(|x| x.as_i64()).unwrap_or(0);
                let dest = if s("destination").is_empty() { String::new() } else { format!(" · {} → {}", s("line"), s("destination")) };
                l.ui.text_in(&format!("{} · {}{dest}{} · {}", s("name"), short_bus(&s("bus")), if pax > 0 { format!(" · {pax} passengers") } else { String::new() }, s("where")), Rect::new(r.x + 42.0, yy, r.w - 60.0, 18.0), 12.5, Weight::Medium, TEXT_SOFT, Align::Left);
                yy += 20.0;
            }
        }
        if !chat.is_empty() {
            l.ui.text_in("CHAT  (V in the game to write)", Rect::new(r.x + 42.0, yy + 4.0, 300.0, 18.0), 10.5, Weight::Black, TEXT_FAINT, Align::Left);
            yy += 26.0;
            for c in chat.iter().rev().take(6).rev() {
                l.ui.text_in(c, Rect::new(r.x + 42.0, yy, r.w - 60.0, 18.0), 12.0, Weight::Regular, TEXT_SOFT, Align::Left);
                yy += 18.0;
            }
        }
        for w in &warnings {
            l.ui.icon("warning", Vec2::new(r.x + 50.0, yy + 9.0), 15.0, WARN);
            l.ui.text_in(w, Rect::new(r.x + 64.0, yy, r.w - 80.0, 18.0), 12.0, Weight::Medium, WARN, Align::Left);
            yy += 20.0;
        }
        if log_open {
            let lr = Rect::new(r.x + 20.0, yy + 6.0, r.w - 40.0, 204.0);
            l.ui.p().rounded(lr, 8.0, Color::rgba(6, 8, 10, 0.9));
            let text: Vec<String> = log_lines.iter().rev().take(11).rev().cloned().collect();
            for (k, line) in text.iter().enumerate() {
                l.ui.text_in(line, Rect::new(lr.x + 10.0, lr.y + 6.0 + k as f32 * 18.0, lr.w - 20.0, 18.0), 11.0, Weight::Regular, if line.contains("ERROR") { DANGER } else if line.contains("WARN") { WARN } else { TEXT_DIM }, Align::Left);
            }
        }
        y += h + GAP;
    }
    l.ui.pop_clip();
    // scrolling the list
    let content = y + l.ui.scroll.get(&id_of("sessions")).copied().unwrap_or(0.0) - body.y;
    if l.ui.hover(view) && l.ui.input.wheel.y.abs() > 0.0 {
        let s = l.ui.scroll.entry(id_of("sessions")).or_insert(0.0);
        *s = (*s - l.ui.input.wheel.y * 40.0).clamp(0.0, (content - view.h).max(0.0));
    }
    for (pid, what) in actions {
        match what {
            "stop" => l.state.stop(pid),
            _ => {
                if !l.state.open_logs.remove(&pid) {
                    l.state.open_logs.insert(pid);
                    l.state.log_tail(pid);
                }
            }
        }
    }
    if let Some(c) = copy {
        l.ui.clipboard_out = Some(c);
        l.state.set_status("Session code copied.", false);
    }
    let _ = hhmm;
}

// --- mods ----------------------------------------------------------------------------------------

pub fn mods(l: &mut Launcher, area: Rect) {
    if !l.state.mods_asked {
        l.state.load_mods();
    }
    let body = l.page_title(area, "Mods", "A bus, a map, scenery, a whole OMSI folder - as a folder or a .zip, .7z or .rar. The original OMSI 2 folder is never written to.");
    let cols = 3;
    let cw = (body.w - GAP * 2.0 * (cols as f32 - 1.0)) / cols as f32;
    let colr = |k: usize| Rect::new(body.x + k as f32 * (cw + GAP * 2.0), body.y, cw, body.h);
    // install
    let c0 = colr(0);
    l.ui.panel(c0);
    let inner = l.ui.heading(Rect::new(c0.x + 18.0, c0.y + 14.0, c0.w - 36.0, c0.h - 28.0), "Install a mod", Some("download"));
    let mut y = inner.y;
    let half = (inner.w - GAP) * 0.5;
    if l.ui.button("mod-folder", Rect::new(inner.x, y, half, 40.0), "Choose a folder", Some("folder_open"), ButtonKind::Primary) {
        if super::mobile::mobile() {
            l.browse(super::mobile::Purpose::ModFolder, "");
        } else if let Some(p) = core::pick_mod(false) {
            l.state.install(p.to_string_lossy().to_string());
        }
    }
    if l.ui.button("mod-zip", Rect::new(inner.x + half + GAP, y, half, 40.0), "Choose archive", Some("inventory_2"), ButtonKind::Normal) {
        if super::mobile::mobile() {
            l.browse(super::mobile::Purpose::ModZip, "");
        } else if let Some(p) = core::pick_mod(true) {
            l.state.install(p.to_string_lossy().to_string());
        }
    }
    y += 52.0;
    l.ui.label(Rect::new(inner.x, y, inner.w, 20.0), "Archive install mode");
    y += 22.0;
    let mut m = l.state.mod_mode;
    if l.ui.segmented("mod-mode", Rect::new(inner.x, y, inner.w, 34.0), &mut m, &["Auto", "Unpacked", "Used in place"]) {
        l.state.mod_mode = m;
    }
    y += 44.0;
    let drop = Rect::new(inner.x, y, inner.w, 110.0);
    let hot = l.pages.drop_hover;
    let t = l.ui.anim(id_of("drop"), if hot { 1.0 } else { 0.0 }, 0.1);
    l.ui.p().rounded(drop, 12.0, ACCENT.alpha(0.05 + 0.12 * t));
    // a dashed edge
    let per = 2.0 * (drop.w + drop.h);
    let n = (per / 14.0) as usize;
    for k in 0..n {
        let s = k as f32 * per / n as f32;
        let p = if s < drop.w {
            Vec2::new(drop.x + s, drop.y)
        } else if s < drop.w + drop.h {
            Vec2::new(drop.right(), drop.y + s - drop.w)
        } else if s < 2.0 * drop.w + drop.h {
            Vec2::new(drop.right() - (s - drop.w - drop.h), drop.bottom())
        } else {
            Vec2::new(drop.x, drop.bottom() - (s - 2.0 * drop.w - drop.h))
        };
        l.ui.p().circle(p, 1.3, ACCENT.alpha(0.35 + 0.5 * t));
    }
    l.ui.icon("upload", Vec2::new(drop.center().x, drop.y + 38.0), 30.0, ACCENT.alpha(0.6 + 0.4 * t));
    l.ui.text_in("…or drop a mod folder or .zip, .7z or .rar onto this window", Rect::new(drop.x, drop.y + 62.0, drop.w, 30.0), 12.5, Weight::Medium, TEXT_SOFT, Align::Center);
    y += 122.0;
    if !l.state.mod_path.is_empty() {
        let p = l.state.mod_path.clone();
        y += l.ui.paragraph(&p, Vec2::new(inner.x, y), inner.w, 11.5, Weight::Regular, TEXT_FAINT);
        match l.state.mod_info.clone() {
            Some(Ok(i)) if i.is_archive => {
                let fit = if i.fits { format!("fits ({} free)", fmt_bytes(i.free_bytes)) } else { format!("does not fit: needs {}, {} free", fmt_bytes(i.needed_bytes), fmt_bytes(i.free_bytes)) };
                let place = if i.in_place_ok { "can be used in place".to_string() } else { i.in_place.clone() };
                y += l.ui.paragraph(&format!("{} archive, {} files, {} unpacked - {fit}; {place}", fmt_bytes(i.archive_bytes), i.files, fmt_bytes(i.unpacked_bytes)), Vec2::new(inner.x, y), inner.w, 12.0, Weight::Regular, if i.fits { TEXT_DIM } else { WARN });
            }
            Some(Err(e)) => {
                y += l.ui.paragraph(&e, Vec2::new(inner.x, y), inner.w, 12.0, Weight::Regular, DANGER);
            }
            _ => {}
        }
    }
    y += 10.0;
    if let Some(m) = l.state.mods.clone() {
        l.ui.heading(Rect::new(inner.x, y, inner.w, 28.0), "The Mods folder", None);
        y += 30.0;
        y += l.ui.paragraph(&format!("Anything put into {} is installed by itself once it has finished copying.", m.inbox), Vec2::new(inner.x, y), inner.w, 12.0, Weight::Regular, TEXT_DIM);
        if !m.inbox_items.is_empty() {
            l.ui.paragraph(&format!("In it now: {}", m.inbox_items.join(", ")), Vec2::new(inner.x, y + 4.0), inner.w, 12.0, Weight::Regular, TEXT_SOFT);
        }
    }
    // installs
    let c1 = colr(1);
    l.ui.panel(c1);
    let inner = l.ui.heading(Rect::new(c1.x + 18.0, c1.y + 14.0, c1.w - 36.0, c1.h - 28.0), "Installs", Some("inventory_2"));
    if l.ui.button("jobs-clear", Rect::new(c1.right() - 18.0 - 130.0, c1.y + 12.0, 130.0, 30.0), "Clear finished", None, ButtonKind::Ghost) {
        core::install::clear_finished();
        l.state.poll_now();
    }
    let jobs = l.state.jobs.clone();
    let mut cancel = None;
    l.ui.scroll_area("jobs", Rect::new(inner.x - 6.0, inner.y, inner.w + 12.0, inner.h), &mut |ui, v| {
        if jobs.is_empty() {
            ui.paragraph("Nothing installed since the launcher started. Big archives are checked against the free disk space before anything is unpacked; a cancelled or failed install leaves nothing behind.", Vec2::new(v.x + 6.0, v.y), v.w - 12.0, 12.5, Weight::Regular, TEXT_DIM);
            return 60.0;
        }
        let mut y = v.y;
        for j in &jobs {
            let running = j.finished.is_none();
            let msg_h = ui.paragraph_height(&j.message, v.w - 40.0, 12.0, Weight::Regular);
            let h = 50.0 + msg_h + if running { 44.0 } else { 0.0 } + j.warnings.len() as f32 * 18.0;
            let r = Rect::new(v.x + 6.0, y, v.w - 16.0, h);
            ui.p().rounded(r, 10.0, Color::WHITE.alpha(0.04));
            ui.text_in(&j.name, Rect::new(r.x + 12.0, r.y + 8.0, r.w - 120.0, 20.0), 13.5, Weight::Bold, TEXT, Align::Left);
            let sc = match j.state.as_str() {
                "done" => OK,
                "failed" => DANGER,
                "cancelled" => TEXT_DIM,
                _ => ACCENT,
            };
            ui.badge(Vec2::new(r.right() - 90.0, r.y + 10.0), &j.state.to_uppercase(), sc);
            let mut yy = r.y + 34.0;
            if running {
                let frac = if j.bytes_total > 0 { j.bytes_done as f32 / j.bytes_total as f32 } else if j.files_total > 0 { j.files_done as f32 / j.files_total as f32 } else { 0.0 };
                ui.progress(Rect::new(r.x + 12.0, yy, r.w - 24.0, 8.0), frac, true);
                ui.text_in(&format!("{} / {} files · {} / {}", j.files_done, j.files_total, fmt_bytes(j.bytes_done), fmt_bytes(j.bytes_total)), Rect::new(r.x + 12.0, yy + 10.0, r.w - 24.0, 16.0), 11.0, Weight::Regular, TEXT_DIM, Align::Left);
                yy += 30.0;
            }
            yy += ui.paragraph(&j.message, Vec2::new(r.x + 12.0, yy), r.w - 24.0, 12.0, Weight::Regular, if j.state == "failed" { DANGER } else { TEXT_SOFT });
            for w in &j.warnings {
                ui.text_in(&format!("⚠ {w}"), Rect::new(r.x + 12.0, yy, r.w - 24.0, 16.0), 11.0, Weight::Regular, WARN, Align::Left);
                yy += 18.0;
            }
            if running && ui.button(&format!("cancel-{}", j.id), Rect::new(r.x + 12.0, r.bottom() - 34.0, 100.0, 28.0), "Cancel", None, ButtonKind::Danger) {
                cancel = Some(j.id);
            }
            y += h + 10.0;
        }
        y - v.y
    });
    if let Some(id) = cancel {
        core::install::cancel(id);
        l.state.poll_now();
    }
    // content folder
    let c2 = colr(2);
    l.ui.panel(c2);
    let inner = l.ui.heading(Rect::new(c2.x + 18.0, c2.y + 14.0, c2.w - 36.0, c2.h - 28.0), "Content folder", Some("folder_open"));
    let Some(m) = l.state.mods.clone() else {
        l.ui.text_in("Reading…", Rect::new(inner.x, inner.y, inner.w, 20.0), 12.5, Weight::Regular, TEXT_DIM, Align::Left);
        return;
    };
    let mut y = inner.y;
    y += l.ui.paragraph(&m.content_dir, Vec2::new(inner.x, y), inner.w, 12.0, Weight::Medium, TEXT_SOFT);
    y += 6.0;
    l.ui.text_in(&format!("{} free on this disk", fmt_bytes(m.free_bytes)), Rect::new(inner.x, y, inner.w, 20.0), 13.0, Weight::Bold, ACCENT, Align::Left);
    y += 30.0;
    for (f, n) in &m.folders {
        l.ui.icon("folder_open", Vec2::new(inner.x + 9.0, y + 10.0), 16.0, TEXT_DIM);
        l.ui.text_in(f, Rect::new(inner.x + 26.0, y, inner.w * 0.6, 20.0), 12.5, Weight::Medium, TEXT, Align::Left);
        l.ui.text_in(&format!("{n} {}", if *n == 1 { "entry" } else { "entries" }), Rect::new(inner.x, y, inner.w, 20.0), 12.0, Weight::Regular, TEXT_DIM, Align::Right);
        y += 24.0;
    }
    if !m.archives.is_empty() {
        y += 8.0;
        l.ui.heading(Rect::new(inner.x, y, inner.w, 28.0), "Archives used in place", None);
        y += 30.0;
        for (n, b) in &m.archives {
            l.ui.text_in(&format!("{n}  ({})", fmt_bytes(*b)), Rect::new(inner.x, y, inner.w, 20.0), 12.0, Weight::Regular, TEXT_SOFT, Align::Left);
            y += 22.0;
        }
    }
    if !m.waiting.is_empty() {
        y += 8.0;
        l.ui.heading(Rect::new(inner.x, y, inner.w, 28.0), "Waiting for their bus", None);
        y += 30.0;
        for w in &m.waiting {
            l.ui.text_in(w, Rect::new(inner.x, y, inner.w, 20.0), 12.0, Weight::Regular, TEXT_DIM, Align::Left);
            y += 22.0;
        }
    }
}

// --- setup -----------------------------------------------------------------------------------------

pub fn setup(l: &mut Launcher, area: Rect) {
    let body = l.page_title(area, "Setup", "Where the original game and this one are.");
    let r = Rect::new(body.x, body.y, body.w.min(820.0), 300.0);
    l.ui.panel(r);
    let inner = l.ui.heading(Rect::new(r.x + 20.0, r.y + 16.0, r.w - 40.0, r.h - 32.0), "Folders", Some("folder_open"));
    let mut root = l.pages.setup_root.clone().unwrap_or_else(|| l.state.config.root.clone());
    let mut game = l.pages.setup_game.clone().unwrap_or_else(|| l.state.config.game.clone());
    let mut y = inner.y;
    l.ui.label(Rect::new(inner.x, y, 150.0, ROW), "OMSI 2 folder");
    if l.ui.text_input("cfg-root", Rect::new(inner.x + 150.0, y, inner.w - 150.0 - 110.0, ROW), &mut root, "/path/to/OMSI 2", Some("folder_open")) {
        l.pages.setup_root = Some(root.clone());
    }
    if l.ui.button("browse-root", Rect::new(inner.right() - 100.0, y, 100.0, ROW), "Browse", None, ButtonKind::Normal) {
        if super::mobile::mobile() {
            let start = root.clone();
            l.browse(super::mobile::Purpose::Root, &start);
        } else if let Some(p) = core::pick_folder("The OMSI 2 folder (with maps and Vehicles in it)") {
            l.pages.setup_root = Some(p.to_string_lossy().to_string());
        }
    }
    y += ROW + 10.0;
    if core::IN_PROCESS_GAMES {
        // (a phone: the game is this app itself)
        y += l.ui.paragraph("Copy the whole OMSI 2 folder (with maps and Vehicles in it) onto the phone - by cable, from a PC or a USB stick - for example as openOMSI/OMSI 2 in the internal storage, then choose it here with Browse. Mods go into openOMSI/Mods or are installed from the Mods page.", Vec2::new(inner.x, y), inner.w, 12.5, Weight::Regular, TEXT_DIM);
    } else {
        l.ui.label(Rect::new(inner.x, y, 150.0, ROW), "Game binary");
        if l.ui.text_input("cfg-game", Rect::new(inner.x + 150.0, y, inner.w - 150.0 - 110.0, ROW), &mut game, "openomsi", Some("terminal")) {
            l.pages.setup_game = Some(game.clone());
        }
        if l.ui.button("browse-game", Rect::new(inner.right() - 100.0, y, 100.0, ROW), "Browse", None, ButtonKind::Normal) {
            if let Some(p) = core::pick_file("The openomsi program") {
                l.pages.setup_game = Some(p.to_string_lossy().to_string());
            }
        }
        y += ROW + 16.0;
        y += l.ui.paragraph("The OMSI 2 folder is the one with maps and Vehicles in it (any complete installation). The game binary is the openomsi program; it is found by itself when it sits next to the launcher.", Vec2::new(inner.x, y), inner.w, 12.5, Weight::Regular, TEXT_DIM);
    }
    y += 12.0;
    if l.ui.button("cfg-save", Rect::new(inner.x, y, 180.0, 42.0), "Save", Some("save"), ButtonKind::Primary) {
        l.state.config.root = root.trim().to_string();
        l.state.config.game = game.trim().to_string();
        match core::save_config(&l.state.config) {
            Ok(()) => {
                // (what was read of the folders before is forgotten: a folder copied or
                // changed while the launcher ran is read as it is now)
                omsi_cfg::content_changed();
                l.state.config = core::load_config();
                l.pages.setup_root = None;
                l.pages.setup_game = None;
                l.state.set_status("Saved. Reading the content again…", false);
                l.state.load_content();
            }
            Err(e) => l.state.set_status(format!("{e:#}"), true),
        }
    }
}


// --- tutorials --------------------------------------------------------------------------------

/// OMSI's four tutorials: what each teaches, and a button to start it.
pub fn tutorials(l: &mut Launcher, area: Rect) {
    let body = l.page_title(area, "Tutorials", "OMSI 2's own lessons: each opens its situation, with its pages beside the picture (Enter turns the page).");
    static LIST: std::sync::OnceLock<Vec<(usize, String, String)>> = std::sync::OnceLock::new();
    let list = LIST.get_or_init(omsi_launcher_lib::tutorials);
    if list.is_empty() {
        l.ui.paragraph("No tutorials were found in the OMSI 2 folder (Tutorials).", Vec2::new(body.x, body.y), body.w, 13.0, Weight::Regular, TEXT_DIM);
        return;
    }
    let cols = 2;
    let cw = (body.w - GAP * 2.0) / cols as f32;
    let ch = ((body.h - GAP * 2.0) / 2.0).min(330.0);
    let mut start = None;
    for (k, (n, title, text)) in list.iter().enumerate() {
        let r = Rect::new(body.x + (k % cols) as f32 * (cw + GAP * 2.0), body.y + (k / cols) as f32 * (ch + GAP * 2.0), cw, ch);
        l.ui.panel(r);
        l.ui.text_in(title, Rect::new(r.x + 18.0, r.y + 14.0, r.w - 36.0, 26.0), 17.0, Weight::Bold, TEXT, Align::Left);
        l.ui.push_clip(Rect::new(r.x + 18.0, r.y + 46.0, r.w - 36.0, r.h - 110.0), 0.0);
        l.ui.paragraph(text, Vec2::new(r.x + 18.0, r.y + 46.0), r.w - 36.0, 12.5, Weight::Regular, TEXT_DIM);
        l.ui.pop_clip();
        if l.ui.button(&format!("tut-{n}"), Rect::new(r.x + 18.0, r.bottom() - 58.0, 200.0, 40.0), "Start the lesson", Some("play_arrow"), ButtonKind::Primary) {
            start = Some(*n);
        }
    }
    if let Some(n) = start {
        l.state.launch_tutorial(n);
    }
}

#[cfg(test)]
mod wizard_tests {
    fn feedback_wizard() -> super::Wizard {
        super::Wizard {
            step: super::WIZARD_STEPS.len(), rest: [Some(0.0); 8],
            at: vec![[Some(-1.0), None, None, None, None, None, None, None], [None; 8], [None; 8], [None; 8]],
            error: None, calibration: None, ff_choice: None, test_strength: crate::ffb_calibration::PULSE_FORCE,
        }
    }

    fn click_feedback(name: &str, w: &mut super::Wizard, d: &mut crate::controllers::DeviceCfg) -> Option<bool> {
        use super::*;
        let mut ui = Ui::new();
        let size = Vec2::new(900.0, 700.0);
        let area = Rect::new(20.0, 20.0, 700.0, 600.0);
        let mut io = None;
        let mut active = false;
        ui.begin(size, 1.0, 0.016);
        feedback_setup(&mut ui, area, w, d, &[], None, &mut io, &mut active, None, false);
        let rect = ui.drawn[&id_of(name)];
        ui.input.mouse = rect.center();
        ui.input.pressed = true;
        ui.input.down = true;
        ui.begin(size, 1.0, 0.016);
        feedback_setup(&mut ui, area, w, d, &[], None, &mut io, &mut active, None, false);
        ui.input.pressed = false;
        ui.input.down = false;
        ui.input.released = true;
        ui.begin(size, 1.0, 0.016);
        let done = feedback_setup(&mut ui, area, w, d, &[], None, &mut io, &mut active, None, false);
        assert!(!active);
        assert!(io.is_none());
        done
    }

    #[test]
    fn manual_direction_is_only_applied_on_finish_and_cancel_preserves_the_device() {
        let original = crate::controllers::DeviceCfg { ff_invert: Some(true), ..Default::default() };
        let mut device = original.clone();
        let mut w = feedback_wizard();
        assert_eq!(click_feedback("wiz-ff-manual", &mut w, &mut device), None);
        assert_eq!(w.ff_choice, Some(false));
        assert_eq!(device, original);
        assert_eq!(click_feedback("wiz-cancel", &mut w, &mut device), Some(false));
        assert_eq!(device, original);
        assert_eq!(click_feedback("wiz-ff-finish", &mut w, &mut device), Some(true));
        assert_eq!(device.ff_invert, Some(false));
        assert_eq!(device.axes[0], Some((Func::Steering, false)));
    }

    #[test]
    fn disconnected_wheel_cannot_start_a_hardware_test() {
        let mut device = crate::controllers::DeviceCfg::default();
        let mut w = feedback_wizard();
        assert_eq!(click_feedback("wiz-ff-test", &mut w, &mut device), None);
        assert!(w.error.is_some());
        assert!(w.calibration.is_none());
    }

    #[test]
    fn cancelling_feedback_releases_io_and_invalidates_the_test() {
        let mut pads = super::PadsView::default();
        pads.feedback_test = true;
        pads.wizard = Some(super::Wizard {
            step: super::WIZARD_STEPS.len(), rest: [None; 8], at: Vec::new(), error: None,
            calibration: Some((std::time::Instant::now(), crate::ffb_calibration::Calibration::new(crate::ffb_calibration::PULSE_FORCE))), ff_choice: None, test_strength: crate::ffb_calibration::PULSE_FORCE,
        });
        pads.cancel_feedback_test();
        assert!(!pads.feedback_test);
        assert!(pads.io.is_none());
        let test = &pads.wizard.as_ref().unwrap().calibration.as_ref().unwrap().1;
        assert!(test.result.unwrap().is_err());
        assert_eq!(pads.wizard.as_ref().unwrap().ff_choice, None);
    }

    use crate::controllers::Func;

    #[test]
    fn h_pattern_gears_have_a_clear_name() {
        assert_eq!(super::known_action("kw_s_1_fest").as_deref(), Some("Gear 1 (H-pattern)"));
        assert_eq!(super::known_action("kw_s_R_fest").as_deref(), Some("Gear R (H-pattern)"));
    }

    #[test]
    fn empty_saved_button_slots_do_not_fill_the_controller_list() {
        let mut buttons = vec![(String::new(), "0".to_string()); 131];
        buttons[10].0 = "horn".to_string();
        assert_eq!(super::shown_button_count(&buttons, 18, None), 18);
        assert_eq!(super::shown_button_count(&buttons, 18, Some(128)), 129);
    }

    #[test]
    fn a_wheel_with_three_pedals() {
        // X the wheel; Y throttle, Z brake, Rz clutch - pedals reading 1 up, -1 down (as the
        // G25's run, "reversed")
        let rest = [Some(0.0), Some(1.0), Some(1.0), None, None, Some(1.0), None, None];
        let mut left = rest;
        left[0] = Some(-1.0);
        let mut thr = rest;
        thr[1] = Some(-1.0);
        let mut brk = rest;
        brk[2] = Some(-1.0);
        let mut clu = rest;
        clu[5] = Some(-1.0);
        let a = super::wizard_result(&rest, &[left, thr, brk, clu]);
        assert_eq!(a[0], Some((Func::Steering, false)));
        assert_eq!(a[1], Some((Func::Throttle, true)));
        assert_eq!(a[2], Some((Func::Brake, true)));
        assert_eq!(a[5], Some((Func::Clutch, true)));
    }

    #[test]
    fn pedals_on_one_axis_and_a_wheel_the_other_way() {
        let rest = [Some(0.0), Some(0.0), None, None, None, None, None, None];
        let a = super::wizard_result(&rest, &[[Some(0.9), Some(0.0), None, None, None, None, None, None], [Some(0.0), Some(-1.0), None, None, None, None, None, None], [Some(0.0), Some(1.0), None, None, None, None, None, None], [None; 8]]);
        assert_eq!(a[0], Some((Func::Steering, true)));
        assert_eq!(a[1], Some((Func::ThrottleBrake, true)));
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;
    use crate::updater::Status;

    /// Every clickable thing of the settings page by the tab it is on (switches are named
    /// `set-<key>`). Taken from the page as it was before the tabs: nothing may go missing.
    fn by_tab() -> Vec<Vec<&'static str>> {
        let mut graphics = vec![
            "s-gp-sel", "s-gp-load", "s-gp-del", "s-gp-name", "s-gp-save",
            "s-preset", "s-graphics", "s-msaa", "s-scale", "s-af", "s-shadow", "set-ssao", "set-shadows", "s-casters", "set-detail_textures", "s-led", "s-led-mip", "set-shadow_blobs", "set-reflections", "set-clouds",
            "set-fullscreen", "s-res", "set-vsync", "s-fps", "s-view", "s-maxobj", "s-minobj", "s-mirror", "s-mirror-refresh", "s-texmem", "set-texture_compression",
        ];
        if !cfg!(target_os = "macos") {
            graphics.push("s-api");
        }
        let driving = vec![
            "s-keys", "set-steering_linear", "set-old_steering", "set-red_steer_spd", "s-mouse", "set-mouse_right_off", "set-blinker_cancel", "set-brake_hold", "set-auto_clutch", "set-momentary_gears", "s-go-keys",
            "s-wrange", "s-wlock", "s-pedt", "s-pedb", "set-ff_enabled", "set-ff_invert", "s-wreset", "s-go-pads",
        ];
        let mut camera = vec![
            "s-seaty",
            "s-seatz",
            "s-seatx",
            "s-seatreset",
            "s-fov",
            "s-look-sens",
            "s-head-idle",
            "s-head-idle-pace",
            "set-steer_look",
            "s-steer-look-angle",
            "s-steer-look-response",
            "set-head_movement",
            "set-driverview_smooth",
            "set-hands_in_cab",
            "set-alt_view",
            "set-camera_collision",
            "set-driver",
            "set-head_tracking",
            "set-triple_screen",
            "set-triple_span",
            "set-triple_hud_center",
            "s-triple-width_mm",
            "s-triple-distance_mm",
            "s-triple-bezel_mm",
            "s-triple-left_angle_deg",
            "s-triple-right_angle_deg",
            "s-triple-eye_height_mm",
        ];
        if cfg!(windows) {
            camera.extend(["set-vr", "s-vr-scale", "s-vr-head-smoothing", "s-vr-mirror-rate", "set-vr_desktop_mirror", "s-go-vr-keys"]);
        }
        let sound = vec!["s-vol", "s-volai", "s-volsc", "set-doppler", "s-voices"];
        let gameplay = vec![
            "s-board", "set-exact_fare", "s-pax", "set-get_up", "s-unsched", "s-maxsched", "s-maxpark",
            "s-maint", "set-collision_vehicles", "set-collision_objects", "set-collision_pedestrians", "set-use_real_time", "set-use_real_date", "set-time_sync", "set-metar_sync", "s-timespeed",
        ];
        let general = vec![
            "s-lang", "set-machine_translation", "set-launcher_rest", "set-discord_status", "set-voice_chat", "s-uiscale", "set-ui_scale_window", "s-uiop", "set-tooltips", "set-show_fps", "set-notes", "set-chat", "set-name_tags",
            "set-navigator", "set-nav_arrows", "set-nav_ai", "corner-top-left", "corner-top-right", "corner-bottom-left", "corner-bottom-right",
            "set-update_check", "set-update_auto", "s-upd-check", "s-upd-github", "s-reset",
        ];
        vec![graphics, driving, camera, sound, gameplay, general]
    }

    /// Settings that show every row: Enhanced (Vanilla hides the shadows and effects), VR on.
    fn all_rows() -> Value {
        let mut s = core::settings_from_text(None);
        s["triple_screen"] = json!(true);
        s["graphics"] = json!("enhanced");
        s["vr"] = json!(true);
        s
    }

    fn outside() -> Outside {
        Outside { update: Status::Idle, check_updates: false, reset: false, controls: None }
    }

    /// One frame of tab `tab`, its two columns tall enough that nothing is cut off.
    fn frame(ui: &mut Ui, tab: usize, s: &mut Value, out: &mut Outside) {
        ui.begin(Vec2::new(1200.0, 2000.0), 1.0, 1.0 / 60.0);
        let mut dirty = 0.0;
        settings_tab(ui, tab, s, &mut dirty, out, [Rect::new(0.0, 0.0, 580.0, 2000.0), Rect::new(620.0, 0.0, 580.0, 2000.0)]);
    }

    /// Click the widget `name` on tab `tab`: the mouse goes down over it and comes up again.
    fn click(tab: usize, name: &str, s: &mut Value) -> Outside {
        let mut ui = Ui::new();
        let mut out = outside();
        frame(&mut ui, tab, s, &mut out);
        let r = *ui.drawn.get(&id_of(name)).unwrap_or_else(|| panic!("{name} is not on the {} tab", SETTINGS_TABS[tab]));
        ui.input.mouse = r.center();
        ui.input.pressed = true;
        ui.input.down = true;
        frame(&mut ui, tab, s, &mut out);
        ui.input.pressed = false;
        ui.input.down = false;
        ui.input.released = true;
        frame(&mut ui, tab, s, &mut out);
        out
    }

    #[test]
    fn every_setting_is_on_exactly_one_tab() {
        let tabs = by_tab();
        assert_eq!(tabs.len(), SETTINGS_TABS.len());
        let mut seen = std::collections::HashSet::new();
        for name in tabs.iter().flatten() {
            assert!(seen.insert(*name), "{name} is listed on two tabs");
        }
        for (tab, names) in tabs.iter().enumerate() {
            let mut ui = Ui::new();
            frame(&mut ui, tab, &mut all_rows(), &mut outside());
            for name in names {
                assert!(ui.drawn.contains_key(&id_of(name)), "{name} is not on the {} tab", SETTINGS_TABS[tab]);
            }
            assert_eq!(ui.drawn.len(), names.len(), "the {} tab has a clickable thing more than the list names", SETTINGS_TABS[tab]);
        }
    }

    #[test]
    fn the_driving_tab_leads_to_the_keys_and_the_controllers() {
        let mut s = all_rows();
        assert_eq!(click(1, "s-go-keys", &mut s).controls, Some(0));
        assert_eq!(click(1, "s-go-pads", &mut s).controls, Some(1));
    }

    #[test]
    fn reset_asks_first_and_changes_nothing() {
        let mut s = all_rows();
        let before = s.clone();
        let out = click(5, "s-reset", &mut s);
        assert!(out.reset);
        assert_eq!(s, before);
    }
}
