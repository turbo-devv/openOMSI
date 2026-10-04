//! The launcher's side of the updates (see `crate::updater`): the check when it starts,
//! the question, the progress, and the restart.

use super::theme::*;
use super::ui::ButtonKind;
use super::Launcher;
use crate::updater::{self, Status};
use glam::Vec2;
use omsi_ui::paint::Align;
use omsi_ui::{Rect, Weight};
use winit::event_loop::ActiveEventLoop;

impl Launcher {
    fn setting(&self, key: &str, default: bool) -> bool {
        self.state.settings.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
    }

    /// Once a frame (drawn or not): look for an update when the launcher has started, install
    /// one when that is what the player chose, and on a computer hand over to the new
    /// launcher once it is in place.
    pub(super) fn update_tick(&mut self, event_loop: &ActiveEventLoop) {
        if !self.update.checked_once && self.started.elapsed().as_secs_f32() > 1.0 {
            if self.setting("update_check", true) && omsi_cfg::env::var_os("OMSI_NO_UPDATE").is_none() {
                self.update.check();
            } else {
                self.update.checked_once = true;
            }
        }
        self.update.poll();
        match self.update.status() {
            // "install updates without asking"
            Status::Available(r) if self.setting("update_auto", false) && !self.update.dismissed && !self.update.auto_started => {
                self.update.auto_started = true;
                log::info!("update: installing {} by itself (update_auto)", r.version);
                self.update.install(r);
            }
            Status::Restarting(r) => {
                if !self.update.relaunched {
                    self.update.relaunched = true;
                    match updater::install_place().and_then(|p| updater::relaunch(&p)) {
                        Ok(()) => {
                            log::info!("update: {} installed, the new launcher starts", r.version);
                            event_loop.exit();
                        }
                        Err(e) => self.state.set_status(format!("openOMSI {} is installed; start it again yourself ({e}).", r.version), true),
                    }
                }
            }
            _ => {}
        }
    }

    /// Whether the update dialog lies over the page this frame.
    pub(super) fn update_dialog_open(&self) -> bool {
        match self.update.status() {
            Status::Available(_) | Status::Failed(_) => !self.update.dismissed,
            Status::Downloading { .. } | Status::Installing(_) | Status::WaitingForInstaller(_) | Status::Restarting(_) => true,
            _ => false,
        }
    }

    /// The update dialog over the page.
    pub(super) fn draw_update_dialog(&mut self) {
        let status = self.update.status();
        let size = self.ui.size;
        let full = Rect::new(0.0, 0.0, size.x, size.y);
        self.ui.solid(full);
        self.ui.p().rect(full, omsi_ui::Color::rgba(0, 0, 0, 0.62));
        let w = (size.x - 48.0).min(560.0);
        let h = 250.0;
        let r = Rect::new((size.x - w) * 0.5, (size.y - h) * 0.5, w, h);
        self.ui.panel(r);
        let inner = Rect::new(r.x + 24.0, r.y + 20.0, r.w - 48.0, r.h - 40.0);
        let current = updater::current_version();
        let icon_at = Vec2::new(inner.x + 14.0, inner.y + 14.0);
        let title_r = Rect::new(inner.x + 38.0, inner.y, inner.w - 38.0, 28.0);
        let body_at = Vec2::new(inner.x, inner.y + 44.0);
        let buttons_y = inner.bottom() - 40.0;
        match status {
            Status::Available(rel) => {
                self.ui.icon("system_update", icon_at, 26.0, ACCENT);
                self.ui.text_in(&format!("openOMSI {} is available", rel.version), title_r, 18.0, Weight::Bold, TEXT, Align::Left);
                let text = if cfg!(target_os = "android") {
                    format!("You have {current}. Update now? The launcher downloads the new version ({}) from GitHub and Android installs it; openOMSI then starts again - your mods and settings stay as they are.", mb(rel.size))
                } else {
                    format!("You have {current}. Update now? The launcher downloads the new version ({}) from GitHub, puts it in place of this one and starts again - your mods and settings stay as they are.", mb(rel.size))
                };
                self.ui.paragraph(&text, body_at, inner.w, 13.0, Weight::Regular, TEXT_DIM);
                let mut auto = self.setting("update_auto", false);
                if self.ui.toggle("upd-auto", Rect::new(inner.x, buttons_y - 44.0, inner.w, 30.0), &mut auto, "Install updates without asking from now on") {
                    self.state.settings["update_auto"] = serde_json::json!(auto);
                    self.state.settings_dirty = 0.3;
                }
                if self.ui.button("upd-now", Rect::new(inner.right() - 150.0, buttons_y, 150.0, 38.0), "Update now", Some("download"), ButtonKind::Primary) {
                    self.update.install(rel.clone());
                }
                if self.ui.button("upd-later", Rect::new(inner.right() - 270.0, buttons_y, 110.0, 38.0), "Not now", None, ButtonKind::Normal) {
                    self.update.dismiss();
                }
                if self.ui.button("upd-page", Rect::new(inner.x, buttons_y, 150.0, 38.0), "What's new", Some("open_in_new"), ButtonKind::Ghost) {
                    updater::open_url(&rel.page);
                }
            }
            Status::Downloading { release, done, total } => {
                self.ui.icon("download", icon_at, 26.0, ACCENT);
                self.ui.text_in(&format!("Downloading openOMSI {}", release.version), title_r, 18.0, Weight::Bold, TEXT, Align::Left);
                let frac = if total > 0 { done as f32 / total as f32 } else { 0.0 };
                self.ui.paragraph(&format!("{} of {} from github.com/{}", mb(done), mb(total), updater::REPO), body_at, inner.w, 13.0, Weight::Regular, TEXT_DIM);
                self.ui.progress(Rect::new(inner.x, body_at.y + 40.0, inner.w, 10.0), frac, true);
            }
            Status::Installing(release) | Status::Restarting(release) => {
                self.ui.icon("install_desktop", icon_at, 26.0, ACCENT);
                self.ui.text_in(&format!("Installing openOMSI {}", release.version), title_r, 18.0, Weight::Bold, TEXT, Align::Left);
                self.ui.paragraph("The new version is put in place; the launcher starts again in a moment.", body_at, inner.w, 13.0, Weight::Regular, TEXT_DIM);
                self.ui.progress(Rect::new(inner.x, body_at.y + 40.0, inner.w, 10.0), 1.0, true);
            }
            Status::WaitingForInstaller(release) => {
                self.ui.icon("install_mobile", icon_at, 26.0, ACCENT);
                self.ui.text_in(&format!("Installing openOMSI {}", release.version), title_r, 18.0, Weight::Bold, TEXT, Align::Left);
                self.ui.paragraph("Android asks whether to update openOMSI: press Update there. The app then starts again by itself.", body_at, inner.w, 13.0, Weight::Regular, TEXT_DIM);
                self.ui.progress(Rect::new(inner.x, body_at.y + 60.0, inner.w, 10.0), 1.0, true);
            }
            Status::Failed(msg) => {
                self.ui.icon("error", icon_at, 26.0, DANGER);
                self.ui.text_in("Not updated", title_r, 18.0, Weight::Bold, TEXT, Align::Left);
                self.ui.paragraph(&msg, body_at, inner.w, 13.0, Weight::Regular, TEXT_DIM);
                if self.ui.button("upd-close", Rect::new(inner.right() - 110.0, buttons_y, 110.0, 38.0), "Close", None, ButtonKind::Normal) {
                    self.update.dismiss();
                }
                if self.ui.button("upd-retry", Rect::new(inner.right() - 240.0, buttons_y, 120.0, 38.0), "Try again", Some("refresh"), ButtonKind::Normal) {
                    self.update.check();
                }
                if self.ui.button("upd-github", Rect::new(inner.x, buttons_y, 170.0, 38.0), "Open on GitHub", Some("open_in_new"), ButtonKind::Ghost) {
                    updater::open_url(&format!("{}/releases/latest", updater::REPO_URL));
                }
            }
            _ => {}
        }
    }
}

impl Launcher {
    /// After an update: "Updated to openOMSI x" in the top right corner for a few seconds
    /// (it asks nothing and covers nothing that matters).
    pub(super) fn draw_updated_notice(&mut self) {
        let Some((v, at)) = self.update.updated.clone() else { return };
        let t = at.elapsed().as_secs_f32();
        if t > 9.0 {
            self.update.updated = None;
            return;
        }
        let fade = (t / 0.3).min(1.0).min((9.0 - t) / 0.6).clamp(0.0, 1.0);
        let text = format!("Updated to openOMSI {v}");
        let w = self.ui.width(&text, 13.5, Weight::Bold) + 60.0;
        let r = Rect::new(self.ui.size.x - w - 20.0, 18.0, w, 42.0);
        self.ui.p().rounded(r, 8.0, PANEL.alpha(0.97 * fade));
        self.ui.p().rounded_border(r, 8.0, 1.0, ACCENT.alpha(0.6 * fade));
        self.ui.icon("check_circle", Vec2::new(r.x + 22.0, r.center().y), 20.0, ACCENT.alpha(fade));
        self.ui.text_in(&text, Rect::new(r.x + 40.0, r.y, w - 48.0, r.h), 13.5, Weight::Bold, TEXT.alpha(fade), Align::Left);
    }
}

/// The run went down while a Vulkan driver compiled the shaders: the LAST it said was a stage
/// of that, and it drew with Vulkan (as the phone's shell decides it, `android.rs`). Any
/// compile stage anywhere in the log said so of every silent end - a phone run out of memory
/// 75 % into loading a map on OpenGL was told its Vulkan driver had failed (#848).
pub(crate) fn died_compiling_on_vulkan(log: &str) -> bool {
    let last = log.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
    let compiling = last.contains("renderer: compiling") || last.contains("cloud noise made") || last.contains("opening graphics device") || last.contains("compiling renderer pipelines");
    let vulkan = log.lines().any(|l| (l.contains("renderer: ") || l.contains("opening graphics device")) && l.contains("(Vulkan")) || log.lines().any(|l| l.contains("graphics: ") && l.to_ascii_uppercase().contains("VULKAN"));
    compiling && vulkan
}

#[cfg(test)]
mod hint_tests {
    #[test]
    fn only_a_run_that_died_compiling_on_vulkan_is_told_so() {
        let compiled = "[t INFO r] opening graphics device: Mali (Vulkan, vendor 0x13b5)\n[t INFO r] renderer: compiling the scene shaders\n";
        assert!(super::died_compiling_on_vulkan(compiled));
        // compiled long ago, then ran out of memory loading
        assert!(!super::died_compiling_on_vulkan(&format!("{compiled}[t INFO g] status: 63 fps, view driver\n[t INFO m] loading tiles 75 %\n")));
        // on OpenGL it is never the Vulkan driver
        assert!(!super::died_compiling_on_vulkan("[t INFO r] opening graphics device: Mali (Gl, vendor 0x13b5)\n[t INFO r] renderer: compiling the scene shaders\n"));
    }
}

impl Launcher {
    /// A game started from here ended on an error: what it said, and the ways to report it
    /// (the end of its log copied, or a GitHub issue opened with it).
    pub(super) fn draw_crash_dialog(&mut self) {
        let Some((what, tail)) = self.state.crash.clone() else { return };
        let size = self.ui.size;
        let full = Rect::new(0.0, 0.0, size.x, size.y);
        self.ui.solid(full);
        self.ui.p().rect(full, omsi_ui::Color::rgba(0, 0, 0, 0.62));
        let w = (size.x - 48.0).min(640.0);
        let lost = what.contains("graphics device was lost");
        let silent = what.contains("closed without a word");
        let compiling = silent && died_compiling_on_vulkan(&tail);
        let hint = if lost {
            if cfg!(windows) {
                "The graphics driver stopped the game. Updating the graphics driver usually helps; you can also let the game draw with DirectX 12 instead of Vulkan (the button below, or Settings → Graphics API)."
            } else {
                "The graphics driver stopped the game. Updating the graphics driver usually helps; Settings → Graphics API can switch to OpenGL."
            }
        } else if compiling {
            "The Vulkan graphics driver stopped while compiling shaders. Starting the game again will switch to OpenGL (or change it in Settings → Graphics API)."
        } else if silent {
            "The system closed the game while it was running, typically because the device ran out of memory (RAM). Lowering texture resolution or reducing AI traffic in Settings helps prevent memory exhaustion."
        } else {
            "Copy the report (the end of the game's log), or open a GitHub issue with it: it tells what went wrong on this computer."
        };
        let text = format!("{what}\n\n{hint}");
        let th = self.ui.paragraph_height(&text, w - 48.0, 13.0, Weight::Regular).min(size.y * 0.5);
        let h = (140.0 + th).min(size.y - 24.0);
        let r = Rect::new((size.x - w) * 0.5, (size.y - h) * 0.5, w, h);
        self.ui.panel(r);
        let inner = Rect::new(r.x + 24.0, r.y + 20.0, r.w - 48.0, r.h - 40.0);
        self.ui.icon(if silent { "info" } else { "error" }, Vec2::new(inner.x + 14.0, inner.y + 14.0), 26.0, if silent { WARN } else { DANGER });
        let title_text = if silent { "The game was closed by the system" } else { "The game closed on an error" };
        self.ui.text_in(title_text, Rect::new(inner.x + 38.0, inner.y, inner.w - 38.0, 28.0), 18.0, Weight::Bold, TEXT, Align::Left);
        self.ui.push_clip(Rect::new(inner.x, inner.y + 40.0, inner.w, th + 4.0), 0.0);
        self.ui.paragraph(&text, Vec2::new(inner.x, inner.y + 40.0), inner.w, 13.0, Weight::Regular, TEXT_DIM);
        self.ui.pop_clip();
        let by = inner.bottom() - 38.0;
        if self.ui.button("crash-close", Rect::new(inner.right() - 110.0, by, 110.0, 38.0), "Close", None, ButtonKind::Normal) {
            self.state.crash = None;
        }
        if self.ui.button("crash-copy", Rect::new(inner.right() - 270.0, by, 150.0, 38.0), "Copy report", Some("content_copy"), ButtonKind::Primary) {
            self.ui.clipboard_out = Some(format!("openOMSI {} ({})\n{what}\n\n{tail}", updater::current_version(), std::env::consts::OS));
            self.state.set_status("The report is copied: paste it into a GitHub issue or a message.", false);
        }
        let api = self.state.settings.get("graphics_api").and_then(|v| v.as_str()).unwrap_or("auto").to_string();
        if lost && cfg!(windows) && api != "dx12" && self.ui.button("crash-dx12", Rect::new(inner.x + 200.0, by, 170.0, 38.0), "Use DirectX 12", Some("monitor"), ButtonKind::Normal) {
            self.state.settings["graphics_api"] = serde_json::json!("dx12");
            self.state.settings_dirty = 0.3;
            self.state.crash = None;
            self.state.set_status("The game draws with DirectX 12 from the next start (Settings → Graphics API to change it back).", false);
        }
        if silent {
            if self.ui.button("crash-settings", Rect::new(inner.x, by, 150.0, 38.0), "Settings", Some("tune"), ButtonKind::Ghost) {
                self.state.crash = None;
                self.go(super::Page::Settings);
            }
        } else if self.ui.button("crash-issue", Rect::new(inner.x, by, 190.0, 38.0), "Report on GitHub", Some("open_in_new"), ButtonKind::Ghost) {
            let title = format!("Crash: {}", what.chars().take(80).collect::<String>());
            let enc = |t: &str| t.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect::<String>();
            // the end of the log goes with it, as much as a link holds (a report of the
            // last line alone said where the game stopped, never what led there); the whole
            // report is on the clipboard as well
            // (the computer and the map always, see `crash_of`)
            let (machine, end) = tail.split_once(&format!("\n{}\n", super::state::CRASH_TAIL_GAP)).unwrap_or(("", &tail));
            let machine = if machine.is_empty() { String::new() } else { format!("The computer:\n```\n{machine}\n```\n\n") };
            let body_with = |end: &str| format!("openOMSI {} on {}\n\n```\n{what}\n```\n\n{machine}The end of the log:\n```\n{end}\n```\n", updater::current_version(), std::env::consts::OS);
            let lines: Vec<&str> = end.lines().collect();
            let mut shown = 0;
            let body = loop {
                let body = body_with(&lines[lines.len() - shown..].join("\n"));
                if shown >= lines.len() || enc(&body).len() > 6500 {
                    break if shown == 0 { body } else { body_with(&lines[lines.len() - shown.saturating_sub(1)..].join("\n")) };
                }
                shown += 1;
            };
            self.ui.clipboard_out = Some(format!("openOMSI {} ({})\n{what}\n\n{tail}", updater::current_version(), std::env::consts::OS));
            updater::open_url(&format!("{}/issues/new?title={}&body={}", updater::REPO_URL, enc(&title), enc(&body)));
        }
    }
}

fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}
