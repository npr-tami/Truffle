//! Updates in the UI: the daily check, the popup (Update now, Later, Skip this version, Stop
//! checking), the download and restart, the title bar's "Update ready" pill, Settings > About's
//! Updates card and the one "updated" note after an update. The work itself is in `crate::update`.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{Align, Align2, CornerRadius, CursorIcon, Id, Layout, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};

use super::fin::{self, Look};
use super::kit::{self, Kind};
use super::theme::{bold, medium, regular};
use super::TruffleApp;
use crate::model::PlayStatus;
use crate::update::{self, Ask, Found, OURS, Progress};

/// The first automatic check waits this long after the start (the start has enough to do).
const FIRST_CHECK: Duration = Duration::from_secs(30);
/// After a failed automatic check, the next try.
const RETRY: Duration = Duration::from_secs(60 * 60);

type CheckSlot = Arc<Mutex<Option<Result<Option<Found>, String>>>>;

/// The updater's state while Truffle runs.
pub(super) struct Updater {
    started: Instant,
    /// The newest release above this version, from the last check.
    found: Option<Found>,
    /// The popup shows.
    popup: bool,
    /// A check on its way, and whether it was "Check now".
    checking: Option<(CheckSlot, bool)>,
    /// No automatic try before this (after one failed).
    next_try: Option<Instant>,
    /// A download on its way.
    install: Option<Arc<Progress>>,
    /// Why the last install failed (its dialog shows).
    failed: Option<String>,
    /// The "updated to" note was looked at this run.
    noted: bool,
}

impl Default for Updater {
    fn default() -> Self {
        Self { started: Instant::now(), found: None, popup: false, checking: None, next_try: None, install: None, failed: None, noted: false }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The PC's time zone, in seconds ahead of UTC.
fn offset_secs() -> i64 {
    super::links::local_offset_ms() / 1000
}

impl TruffleApp {
    /// What to do about the release found (nothing when none was).
    fn update_ask(&self) -> Ask {
        let Some(found) = &self.updater.found else { return Ask::Nothing };
        update::decide(&found.version, OURS, &self.prefs.skipped_updates, self.prefs.update_remind_after, update::now_secs())
    }

    /// A newer version waits (not skipped): the About dot.
    pub(super) fn update_waiting(&self) -> bool {
        self.update_ask() != Ask::Nothing
    }

    /// Every frame, drawn or not: the "updated" note, the daily check and what comes back.
    pub(super) fn updater_logic(&mut self, ctx: &egui::Context) {
        if !self.updater.noted {
            self.updater.noted = true;
            let last = self.prefs.last_run_version.clone();
            if last.as_deref() != Some(OURS) {
                if last.is_some_and(|l| update::is_newer(OURS, &l)) {
                    self.toast(format!("Truffle is updated to {OURS}"), false);
                }
                self.prefs.last_run_version = Some(OURS.to_owned());
                self.prefs_changed();
            }
        }

        let came = self.updater.checking.as_ref().and_then(|(slot, by_hand)| lock(slot).take().map(|r| (r, *by_hand)));
        if let Some((result, by_hand)) = came {
            self.updater.checking = None;
            self.check_came(result, by_hand);
        }

        let done = self.updater.install.as_ref().and_then(|p| lock(&p.result).take());
        if let Some(result) = done {
            self.updater.install = None;
            self.download_came(ctx, result);
        }

        if self.updater.checking.is_some() || self.updater.install.is_some() || !self.prefs.check_updates {
            return;
        }
        let now = update::now_secs();
        let since_start = self.updater.started.elapsed();
        let retry_at = self.updater.next_try.filter(|t| *t > Instant::now());
        if since_start >= FIRST_CHECK && retry_at.is_none() && update::check_due(true, self.prefs.update_last_check, now) {
            self.start_update_check(ctx, false);
            return;
        }
        // Wake up for the next one (at least once an hour, so a day's check isn't missed).
        let until_due = Duration::from_secs(self.prefs.update_last_check.saturating_add(update::CHECK_EVERY_SECS).saturating_sub(now));
        let wait = until_due.max(FIRST_CHECK.saturating_sub(since_start)).max(retry_at.map_or(Duration::ZERO, |t| t - Instant::now()));
        ctx.request_repaint_after(wait.clamp(Duration::from_secs(1), RETRY));
    }

    /// Asks GitHub in the background ("Check now" when `by_hand`).
    fn start_update_check(&mut self, ctx: &egui::Context, by_hand: bool) {
        let slot: CheckSlot = Arc::default();
        self.updater.checking = Some((slot.clone(), by_hand));
        let ctx = ctx.clone();
        self.runtime.spawn(async move {
            let result = update::check().await.map_err(|e| e.to_string());
            *lock(&slot) = Some(result);
            ctx.request_repaint();
        });
    }

    fn check_came(&mut self, result: Result<Option<Found>, String>, by_hand: bool) {
        match result {
            Ok(found) => {
                self.updater.next_try = None;
                self.prefs.update_last_check = update::now_secs();
                self.prefs_changed();
                self.updater.found = found;
                // "Check now" always shows a newer version, also one skipped before.
                let newer = self.updater.found.as_ref().is_some_and(|f| update::is_newer(&f.version, OURS));
                match (self.update_ask(), by_hand) {
                    (Ask::Popup, _) => self.updater.popup = true,
                    (_, true) if newer => self.updater.popup = true,
                    (_, true) => self.toast(format!("You have the newest Truffle ({OURS})"), false),
                    (_, false) => {}
                }
            }
            Err(e) => {
                log::info!("update check failed: {e}");
                self.updater.next_try = Some(Instant::now() + RETRY);
                if by_hand {
                    self.toast(format!("Couldn't check for updates: {e}"), true);
                }
            }
        }
    }

    /// Later (also ✕, Esc): the pill until tomorrow, then the popup again.
    fn update_later(&mut self) {
        self.updater.popup = false;
        self.updater.failed = None;
        self.prefs.update_remind_after = update::now_secs() + update::LATER_SECS;
        self.prefs_changed();
    }

    fn update_skip(&mut self) {
        self.updater.popup = false;
        if let Some(f) = &self.updater.found
            && !self.prefs.skipped_updates.iter().any(|s| update::same_version(s, &f.version))
        {
            self.prefs.skipped_updates.push(f.version.clone());
            self.prefs_changed();
        }
    }

    fn update_stop(&mut self) {
        self.updater.popup = false;
        self.prefs.check_updates = false;
        self.prefs_changed();
        self.toast("Truffle won't look for updates. Turn it back on in Settings, About", false);
    }

    /// Update now: downloads in the background (the music plays on).
    fn update_now(&mut self, ctx: &egui::Context) {
        self.updater.popup = false;
        self.updater.failed = None;
        let Some(found) = self.updater.found.clone() else { return };
        let exe = match update::current_exe() {
            Ok(exe) => exe,
            Err(e) => {
                self.updater.failed = Some(e.to_string());
                return;
            }
        };
        let dest = update::download_path(&exe);
        // A download left from before (cancelled late) goes first.
        let _ = std::fs::remove_file(&dest);
        let progress = Arc::new(Progress::default());
        self.updater.install = Some(progress.clone());
        let ctx = ctx.clone();
        self.runtime.spawn(async move {
            update::download(found, dest, progress).await;
            ctx.request_repaint();
        });
    }

    /// The download is in and checked: swap the exe, then quit (saving the queue and the spot) and
    /// let `main` start the new one.
    fn download_came(&mut self, ctx: &egui::Context, result: Result<std::path::PathBuf, String>) {
        let new = match result {
            Ok(new) => new,
            Err(e) if e == "Cancelled" => return,
            Err(e) => {
                self.updater.failed = Some(e);
                return;
            }
        };
        let swapped = update::current_exe().and_then(|exe| {
            let backup = update::backup_path(&exe, OURS, Path::exists);
            update::swap(&exe, &new, &backup).map(|()| (exe, backup))
        });
        match swapped {
            Ok((exe, backup)) => {
                log::info!("updated the exe; the old one is kept as {}", backup.display());
                let playing = self.playback.track.is_some() && self.playback.status == PlayStatus::Playing;
                update::restart_after_exit(exe, playing);
                self.quit(ctx);
            }
            Err(e) => {
                let _ = std::fs::remove_file(&new);
                self.updater.failed = Some(format!("{e:#}"));
            }
        }
    }

    /// The popup, the installing dialog and the failure dialog.
    pub(super) fn update_dialogs(&mut self, ctx: &egui::Context) {
        if self.updater.install.is_some() {
            self.installing_dialog(ctx);
        } else if self.updater.failed.is_some() {
            self.failed_dialog(ctx);
        } else if self.updater.popup && self.updater.found.is_some() {
            self.update_popup(ctx);
        }
    }

    fn dialog_width(ctx: &egui::Context, w: f32) -> f32 {
        w.min(ctx.content_rect().width() - 80.0).max(240.0)
    }

    /// "Truffle X is ready": what's new and the four choices.
    fn update_popup(&mut self, ctx: &egui::Context) {
        let Some(found) = self.updater.found.clone() else { return };
        let p = self.palette;
        #[derive(PartialEq)]
        enum Choice {
            None,
            Now,
            Later,
            Skip,
            Stop,
        }
        let mut choice = Choice::None;
        let modal = fin::dialog(ctx, Id::new("update_popup"), |ui| {
            ui.set_width(Self::dialog_width(ctx, 480.0));
            let top = ui.cursor().top();
            fin::dialog_head(ui, &format!("Truffle {} is ready", found.version), Some(&format!("You have {OURS}. What's new:")));
            let x = Rect::from_min_size(pos2(ui.max_rect().right() - 26.0, top - 4.0), vec2(26.0, 26.0));
            if fin::ib(ui, &p, x, Id::new("update_popup_x"), "x", Look::Plain, "Later (Esc)").clicked() {
                choice = Choice::Later;
            }
            ui.add_space(12.0);
            egui::Frame::new().fill(p.main_bg).stroke(Stroke::new(1.0, p.border)).corner_radius(CornerRadius::same(8)).inner_margin(12).show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::ScrollArea::vertical().max_height(220.0).auto_shrink([false, true]).show(ui, |ui| {
                    for (k, notes) in found.notes.iter().enumerate() {
                        if k > 0 {
                            ui.add_space(10.0);
                        }
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("Truffle {}", notes.version)).font(bold(13.0)).color(p.text_strong));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(RichText::new(&notes.date).font(regular(12.0)).color(p.text_weak));
                            });
                        });
                        ui.add_space(4.0);
                        if notes.lines.is_empty() {
                            ui.label(RichText::new("No notes for this one.").font(regular(12.5)).color(p.text_weak));
                        }
                        for line in &notes.lines {
                            ui.add(egui::Label::new(RichText::new(line).font(regular(12.5)).color(p.text)).wrap());
                        }
                    }
                });
            });
            ui.add_space(12.0);
            ui.add(
                egui::Label::new(
                    RichText::new("Truffle restarts by itself (a few seconds) and goes on with the same song, where it was.").font(regular(12.0)).color(p.text_weak),
                )
                .wrap(),
            );
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                if kit::button(ui, &p, "Stop checking for updates", Kind::Quiet).clicked() {
                    choice = Choice::Stop;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if kit::button(ui, &p, "Update now", Kind::Primary).clicked() {
                        choice = Choice::Now;
                    }
                    if kit::button(ui, &p, "Later", Kind::Secondary).on_hover_text("Asks again tomorrow").clicked() {
                        choice = Choice::Later;
                    }
                    if kit::button(ui, &p, "Skip this version", Kind::Secondary).on_hover_text(format!("Never asks about {} again", found.version)).clicked() {
                        choice = Choice::Skip;
                    }
                });
            });
        });
        if choice == Choice::None && modal.should_close() {
            choice = Choice::Later;
        }
        match choice {
            Choice::None => {}
            Choice::Now => self.update_now(ctx),
            Choice::Later => self.update_later(),
            Choice::Skip => self.update_skip(),
            Choice::Stop => self.update_stop(),
        }
    }

    /// "Updating to Truffle X": the download's progress and the steps, and Cancel.
    fn installing_dialog(&mut self, ctx: &egui::Context) {
        let Some(progress) = self.updater.install.clone() else { return };
        let version = self.updater.found.as_ref().map_or_else(String::new, |f| f.version.clone());
        let size = self.updater.found.as_ref().map_or(0, |f| f.size).max(1);
        let p = self.palette;
        let done = progress.done.load(Ordering::Relaxed).min(size);
        let stage = progress.stage.load(Ordering::Relaxed);
        let mut cancel = false;
        fin::dialog(ctx, Id::new("update_installing"), |ui| {
            ui.set_width(Self::dialog_width(ctx, 420.0));
            fin::dialog_head(ui, &format!("Updating to Truffle {version}"), Some("Your music keeps playing until the restart."));
            ui.add_space(16.0);
            let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), Sense::hover());
            let share = done as f32 / size as f32;
            ui.painter().rect_filled(bar, CornerRadius::same(3), p.widget_bg);
            ui.painter().rect_filled(Rect::from_min_size(bar.min, vec2(bar.width() * share, bar.height())), CornerRadius::same(3), p.accent);
            ui.add_space(8.0);
            let mb = |b: u64| b as f64 / 1_048_576.0;
            let words = if stage == 0 { format!("Downloading… {:.1} of {:.1} MB", mb(done), mb(size)) } else { "Checking it…".to_owned() };
            ui.horizontal(|ui| {
                ui.label(RichText::new(words).font(regular(12.0)).color(p.text_weak));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new(format!("{:.0}%", share * 100.0)).font(medium(12.0)).color(p.text_weak));
                });
            });
            ui.add_space(14.0);
            let steps = [
                "Get the new version from GitHub".to_owned(),
                format!("Check it's really Truffle {version} (size and checksum)"),
                "Save your place, swap the exe, restart".to_owned(),
            ];
            for (k, step) in steps.iter().enumerate() {
                let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::hover());
                let dot = pos2(row.left() + 7.0, row.center().y);
                let (fill, text) = match (k as u64).cmp(&stage) {
                    std::cmp::Ordering::Less => (p.accent_ink, p.text),
                    std::cmp::Ordering::Equal => (p.text, p.text_strong),
                    std::cmp::Ordering::Greater => (p.text_faint, p.text_weak),
                };
                if (k as u64) < stage {
                    super::svgicon::paint(ui.painter(), Rect::from_center_size(dot, vec2(13.0, 13.0)), "check", fill, super::svgicon::STROKE);
                } else {
                    ui.painter().circle_stroke(dot, 4.5, Stroke::new(1.5, fill));
                }
                ui.painter().text(pos2(row.left() + 22.0, row.center().y), Align2::LEFT_CENTER, step, regular(12.5), text);
            }
            ui.add_space(14.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if kit::button(ui, &p, "Cancel", Kind::Secondary).clicked() {
                    cancel = true;
                }
            });
        });
        // Only Cancel stops it (a click beside the box doesn't).
        if cancel {
            progress.cancel.store(true, Ordering::Relaxed);
            self.updater.install = None;
        }
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    /// "Couldn't update": why, that nothing changed, and Later or Try again.
    fn failed_dialog(&mut self, ctx: &egui::Context) {
        let Some(why) = self.updater.failed.clone() else { return };
        let p = self.palette;
        let (mut later, mut again) = (false, false);
        let modal = fin::dialog(ctx, Id::new("update_failed"), |ui| {
            ui.set_width(Self::dialog_width(ctx, 420.0));
            let note = format!("{why}. Nothing was changed: Truffle {OURS} still works as before.");
            fin::dialog_head(ui, "Couldn't update", Some(&note));
            ui.add_space(18.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                again = kit::button(ui, &p, "Try again", Kind::Primary).clicked();
                later = kit::button(ui, &p, "Later", Kind::Secondary).clicked();
            });
        });
        if again {
            self.update_now(ctx);
        } else if later || modal.should_close() {
            self.update_later();
        }
    }

    /// The title bar's "Update ready" pill (after Later), its right end at `right`, centred on `cy`.
    /// Returns its left edge (or `right` when it isn't shown). A click opens the popup.
    pub(super) fn update_pill(&mut self, ui: &mut Ui, right: f32, cy: f32) -> f32 {
        if self.update_ask() != Ask::Pill || self.updater.install.is_some() {
            return right;
        }
        let p = self.palette.on_dark();
        let galley = ui.painter().layout_no_wrap("Update ready".to_owned(), medium(12.0), p.text);
        let w = 12.0 + 7.0 + 7.0 + galley.size().x + 12.0;
        let rect = Rect::from_min_size(pos2(right - w, cy - 15.0), vec2(w, 30.0));
        let resp = ui.interact(rect, Id::new("update-pill"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand).on_hover_text("A new Truffle is ready");
        let fill = fin::acc(&p, if resp.hovered() { 0.24 } else { 0.16 });
        ui.painter().rect(rect, CornerRadius::same(15), fill, Stroke::new(1.0, fin::acc(&p, 0.35)), egui::StrokeKind::Inside);
        ui.painter().circle_filled(pos2(rect.left() + 12.0 + 3.5, cy), 3.5, p.accent_ink);
        ui.painter().galley(pos2(rect.left() + 12.0 + 7.0 + 7.0, cy - galley.size().y / 2.0), galley, p.text);
        if resp.clicked() {
            self.updater.popup = true;
        }
        rect.left()
    }

    /// Settings > About > Updates.
    pub(super) fn updates_section(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let now = update::now_secs();
        let ctx = ui.ctx().clone();
        let busy = self.updater.checking.is_some() || self.updater.install.is_some();

        if self.update_waiting()
            && let Some(found) = self.updater.found.clone()
        {
            let later = self.prefs.update_remind_after;
            let note = if later > now {
                format!("You chose Later {}. What's new is behind the title bar's pill", update::when_text(later.saturating_sub(update::LATER_SECS), now, offset_secs()))
            } else {
                format!("You have {OURS}")
            };
            let w = fin::text_button_w(ui, "Update now", false) + fin::text_button_w(ui, "Skip this version", false) + fin::text_button_w(ui, "What's new", false) + 16.0;
            kit::row(ui, &p, &format!("Truffle {} is ready", found.version), Some(&note), false, true, w, |ui| {
                if kit::button_enabled(ui, &p, "Update now", Kind::Primary, !busy).clicked() {
                    self.update_now(&ctx);
                }
                if kit::button(ui, &p, "Skip this version", Kind::Secondary).clicked() {
                    self.update_skip();
                }
                if kit::button(ui, &p, "What's new", Kind::Quiet).clicked() {
                    self.updater.popup = true;
                }
            });
        }

        let mut auto = self.prefs.check_updates;
        let note = "Looks once a day while Truffle is open, and asks before installing anything. Only Truffle's releases on GitHub are read: nothing about you or your music is sent";
        if kit::switch_row(ui, &p, "Check for updates automatically", Some(note), &mut auto, false, true) {
            self.prefs.check_updates = auto;
            self.updater.next_try = None;
            self.prefs_changed();
        }

        let last = self.prefs.update_last_check;
        let note = if self.updater.checking.is_some() {
            "Checking…".to_owned()
        } else if last > 0 {
            format!("Version {OURS}. Last checked {}", update::when_text(last, now, offset_secs()))
        } else {
            format!("Version {OURS}. Not checked yet")
        };
        kit::row(ui, &p, "Check now", Some(&note), false, true, fin::text_button_w(ui, "Check now", false), |ui| {
            if kit::button_enabled(ui, &p, "Check now", Kind::Secondary, !busy).clicked() {
                self.start_update_check(&ctx, true);
            }
        });
    }
}
