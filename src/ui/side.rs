//! Settings > Appearance (theme, accent, app icon, lists, size, banners, wallpaper, colours) and
//! the rows of the Up next tab.

use std::ops::RangeInclusive;
use std::path::Path;

use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{Button, Color32, CornerRadius, Grid, Id, RichText, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use super::{fin, kit, screens};
use super::theme::{ACCENT_PRESETS, Palette, has_custom_colors, regular};
use super::wallpaper::{self, FITS, GRID_W, PAD_GRID_GAP, PAD_H, SLIDES, Slide, THUMB_SIZE};
use super::TruffleApp;
use crate::config::{AppIcon, CustomColors, ThemeMode, Wallpaper};

/// A wallpaper slider's row: the slider, the gap after it and its number's box (Glint's, a little
/// narrower so the rows fit a narrow Settings window).
const SLIDER_W: f32 = 180.0;
const VALUE_GAP: f32 = 10.0;
const VALUE_W: f32 = 60.0;
/// The Picture row's name box, at most (Glint's).
const NAME_W: f32 = 196.0;
/// The title's room in a row whose control takes what's left.
const TITLE_ROOM: f32 = 110.0;

impl TruffleApp {
    /// "App icon": the silver or the black turntable on the window, taskbar and tray, or the one
    /// that stands out on the taskbar. A change shows at once.
    pub(super) fn app_icon_row(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let label = |icon: AppIcon| match icon {
            AppIcon::Automatic => "Automatic",
            AppIcon::Silver => "Silver",
            AppIcon::Black => "Black",
        };
        let mut icon = self.prefs.app_icon;
        kit::row(ui, &p, "App icon", None, false, true, 180.0, |ui| {
            egui::ComboBox::from_id_salt("app_icon_choice").width(170.0).selected_text(label(icon)).show_ui(ui, |ui| {
                for choice in [AppIcon::Automatic, AppIcon::Silver, AppIcon::Black] {
                    fin::choice(ui, &mut icon, choice, label(choice));
                }
            });
        });
        if icon != self.prefs.app_icon {
            // The next pass shows it (see `follow_icon_style`).
            self.prefs.app_icon = icon;
            self.prefs_changed();
        }
    }

    pub(super) fn theme_row(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let label = |mode: ThemeMode| match mode {
            ThemeMode::System => "Same as Windows",
            ThemeMode::Light => "Light",
            ThemeMode::Dark => "Dark",
        };
        let mut theme = self.prefs.theme;
        kit::row(ui, &p, "Theme", None, false, true, 180.0, |ui| {
            egui::ComboBox::from_id_salt("theme_choice").width(170.0).selected_text(label(theme)).show_ui(ui, |ui| {
                for mode in [ThemeMode::System, ThemeMode::Light, ThemeMode::Dark] {
                    fin::choice(ui, &mut theme, mode, label(mode));
                }
            });
        });
        if theme != self.prefs.theme {
            self.prefs.theme = theme;
            self.restyle(ui.ctx());
            self.prefs_changed();
        }
    }

    pub(super) fn match_accent_row(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let mut dynamic = self.prefs.dynamic_accent;
        if kit::switch_row(ui, &p, "Match the accent to the album art", None, &mut dynamic, false, true) {
            self.prefs.dynamic_accent = dynamic;
            self.prefs_changed();
        }
    }

    pub(super) fn accent_row(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let note = self.prefs.dynamic_accent.then_some("When a cover has no clear colour");
        kit::row(ui, &p, "Accent colour", note, false, true, 250.0, |ui| self.accent_swatches(ui));
    }

    /// Song size: a slider with three stops (Default, Medium, Big).
    pub(super) fn song_size_row(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let level = self.prefs.song_size.min(2);
        let note = ["Default", "Medium", "Big"][level as usize];
        let mut v = f32::from(level);
        kit::row(ui, &p, "Song size", Some(note), false, true, 230.0, |ui| {
            kit::line_slider(ui, &p, &mut v, 0.0..=2.0, 150.0, true);
        });
        let snapped = v.round().clamp(0.0, 2.0) as u8;
        if snapped != self.prefs.song_size {
            self.prefs.song_size = snapped;
            self.prefs_changed();
        }
    }

    pub(super) fn motion_row(&mut self, ui: &mut Ui) {
        use crate::config::MotionSpeed;
        let p = self.palette;
        let label = |s: MotionSpeed| match s {
            MotionSpeed::Snappy => "Snappy",
            MotionSpeed::Normal => "Normal",
            MotionSpeed::Relaxed => "Relaxed",
        };
        let mut speed = self.prefs.motion_speed;
        kit::row(ui, &p, "Motion", Some("How quickly things move and fade"), false, true, 180.0, |ui| {
            egui::ComboBox::from_id_salt("motion_choice").width(170.0).selected_text(label(speed)).show_ui(ui, |ui| {
                for choice in [MotionSpeed::Snappy, MotionSpeed::Normal, MotionSpeed::Relaxed] {
                    fin::choice(ui, &mut speed, choice, label(choice));
                }
            });
        });
        if speed != self.prefs.motion_speed {
            self.prefs.motion_speed = speed;
            super::motion::set_speed(ui.ctx(), speed);
            self.prefs_changed();
        }
    }

    /// "Wide screens": pages at most 1800 wide in the middle, across the whole width, or in two
    /// columns. The pages change at once.
    pub(super) fn wide_screens_row(&mut self, ui: &mut Ui) {
        use crate::config::WideScreens;
        let p = self.palette;
        let label = |w: WideScreens| match w {
            WideScreens::Centred => "Centred",
            WideScreens::Fill => "Fill",
            WideScreens::Split => "Split",
        };
        let mut wide = self.prefs.wide_screens;
        kit::row(ui, &p, "Wide screens", Some("How pages use a very wide window"), false, true, 180.0, |ui| {
            egui::ComboBox::from_id_salt("wide_choice").width(170.0).selected_text(label(wide)).show_ui(ui, |ui| {
                for choice in [WideScreens::Centred, WideScreens::Fill, WideScreens::Split] {
                    fin::choice(ui, &mut wide, choice, label(choice));
                }
            });
        });
        if wide != self.prefs.wide_screens {
            self.prefs.wide_screens = wide;
            self.prefs_changed();
        }
    }

    pub(super) fn page_animation_row(&mut self, ui: &mut Ui) {
        use crate::config::PageAnimation;
        let p = self.palette;
        let label = |a: PageAnimation| match a {
            PageAnimation::None => "None",
            PageAnimation::Rise => "Rise",
            PageAnimation::TopFirst => "Top first",
        };
        let mut anim = self.prefs.page_animation;
        kit::row(ui, &p, "Page animation", Some("When you switch pages"), false, true, 180.0, |ui| {
            egui::ComboBox::from_id_salt("page_anim_choice").width(170.0).selected_text(label(anim)).show_ui(ui, |ui| {
                for choice in [PageAnimation::None, PageAnimation::Rise, PageAnimation::TopFirst] {
                    fin::choice(ui, &mut anim, choice, label(choice));
                }
            });
        });
        if anim != self.prefs.page_animation {
            self.prefs.page_animation = anim;
            self.prefs_changed();
        }
    }

    pub(super) fn compact_row(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let mut compact = self.prefs.compact;
        if kit::switch_row(ui, &p, "Compact lists", None, &mut compact, false, true) {
            self.prefs.compact = compact;
            self.restyle(ui.ctx());
            self.prefs_changed();
        }
    }

    /// "Interface size" on the screen the window is on (each screen keeps its own; AUTO goes back
    /// to the one picked for the screen), and "Grow with the window".
    pub(super) fn size_row(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let screen = self.screen_zoom.screen.clone();
        let size = screens::size_on(&self.prefs, screen.as_ref());
        let set = screens::is_set(&self.prefs, screen.as_ref());
        let pct = (self.scale_preview.unwrap_or(size) * 100.0).round();
        let auto = if set { "" } else { " (auto)" };
        let note = format!("{pct:.0}% on this screen{auto}  ·  Ctrl + and Ctrl −");
        // Some(None): back to auto.
        let mut new_size = None;
        kit::row(ui, &p, "Interface size", Some(&note), false, true, 230.0, |ui| {
            let mut scale = self.scale_preview.unwrap_or(size);
            let r = kit::line_slider(ui, &p, &mut scale, 0.75..=2.5, 150.0, true);
            let scale = (scale * 20.0).round() / 20.0;
            if r.dragged() {
                self.scale_preview = Some(scale);
            } else if r.drag_stopped() || r.changed() {
                self.scale_preview = None;
                new_size = Some(Some(scale));
            }
            if set && kit::button(ui, &p, "Auto", kit::Kind::Quiet).on_hover_text("The size picked for this screen").clicked() {
                self.scale_preview = None;
                new_size = Some(None);
            }
        });
        if let Some(new_size) = new_size
            && screens::set_size(&mut self.prefs, screen.as_ref(), new_size)
        {
            self.prefs_changed();
        }
        let mut grow = self.prefs.grow_with_window;
        if kit::switch_row(ui, &p, "Grow with the window", Some("Bigger when the window is wide"), &mut grow, true, true) {
            self.prefs.grow_with_window = grow;
            self.prefs_changed();
        }
    }

    /// Banners are set on each list's page (right-click its header); here they can all go at once.
    pub(super) fn banners_row(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let set = self.prefs.banners.len();
        let note = match set {
            0 => "Right-click a playlist's header to add one".to_owned(),
            1 => "1 playlist has one".to_owned(),
            n => format!("{n} playlists have one"),
        };
        let mut remove_all = false;
        kit::row(ui, &p, "Banners", Some(&note), false, true, 120.0, |ui| {
            remove_all = kit::button_enabled(ui, &p, "Remove all", kit::Kind::Secondary, set > 0).clicked();
        });
        if remove_all {
            self.remove_all_banners();
        }
    }

    /// Glint's Wallpaper card: the picture and the pictures next to it, then how it shows (only
    /// while there is one). Changes apply at once.
    pub(super) fn wallpaper_rows(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let mut w = self.prefs.wallpaper.clone();
        self.picture_row(ui, &mut w);
        // Also after NONE (the last picture's folder): one click goes back.
        if !w.path.is_empty() || self.nearby_folder_known() {
            self.nearby_row(ui, &mut w);
        }
        let mut held = None;
        // How it shows only matters once there is one.
        if !w.path.is_empty() {
            kit::row(ui, &p, "Shows behind", None, false, true, 180.0, |ui| {
                egui::ComboBox::from_id_salt("wallpaper_behind")
                    .width(160.0)
                    .selected_text(if w.whole_window { "The whole window" } else { "The songs" })
                    .show_ui(ui, |ui| {
                        fin::choice(ui, &mut w.whole_window, false, "The songs");
                        fin::choice(ui, &mut w.whole_window, true, "The whole window");
                    });
            });
            let fit = FITS.iter().find(|f| f.0 == w.fit).map_or(FITS[0].1, |f| f.1);
            kit::row(ui, &p, "Fit", None, false, true, 180.0, |ui| {
                egui::ComboBox::from_id_salt("wallpaper_fit").width(160.0).selected_text(fit).show_ui(ui, |ui| {
                    for (choice, name) in FITS {
                        fin::choice(ui, &mut w.fit, choice, name);
                    }
                });
            });
            self.position_row(ui, &mut w);
            for (s, title, min, max) in SLIDES {
                held = self.slide_row(ui, &mut w, s, title, min..=max).or(held);
            }
        }
        // Kept from before this pass's change, for Esc while a slider or the pad is held.
        self.wallpaper_held(held, !w.path.is_empty());
        if w != self.prefs.wallpaper {
            self.prefs.wallpaper = w;
            self.prefs_changed();
            // The panels were painted before these rows: show the change on the next frame.
            ui.ctx().request_repaint();
        }
    }

    /// "Picture": NONE, BROWSE… and the picture's name in a box; why it can't be shown, under the
    /// title.
    fn picture_row(&mut self, ui: &mut Ui, w: &mut Wallpaper) {
        let p = self.palette;
        let set = !w.path.is_empty();
        let note = self.wallpaper_error_note();
        let gap = ui.spacing().item_spacing.x;
        let buttons = fin::text_button_w(ui, "None", false) + gap + fin::text_button_w(ui, "Browse…", false) + gap;
        // The name's box takes what's left beside the title, up to Glint's width.
        let name_w = (ui.available_width() - 56.0 - TITLE_ROOM - buttons).clamp(90.0, NAME_W);
        let picking = self.wallpaper.picking();
        let mut browse = false;
        kit::row(ui, &p, "Picture", note.as_deref(), false, true, name_w + buttons, |ui| {
            // Right to left: the name, BROWSE…, NONE.
            name_box(ui, name_w, &w.path);
            browse = kit::button_enabled(ui, &p, "Browse…", kit::Kind::Secondary, !picking).clicked();
            if kit::button_enabled(ui, &p, "None", kit::Kind::Secondary, set).clicked() {
                w.path.clear();
            }
        });
        if browse {
            let ctx = ui.ctx().clone();
            self.pick_wallpaper(&ctx);
        }
    }

    /// "Nearby": the pictures in the picture's folder (or the last picture's), newest first; a
    /// click uses one.
    fn nearby_row(&mut self, ui: &mut Ui, w: &mut Wallpaper) {
        let p = self.palette;
        // As many as fit beside the title.
        let room = (ui.available_width() - 56.0 - TITLE_ROOM).max(THUMB_SIZE.x);
        let picked = kit::tall_row(ui, &p, "Nearby", Some("Click one to use it"), true, THUMB_SIZE.y + 2.0, room, |ui| self.nearby_thumbs(ui));
        if let Some(path) = picked {
            w.path = path.to_string_lossy().into_owned();
        }
    }

    /// "Position": the pad to drag the picture on, and the 3x3 squares (corners, edges, middle).
    fn position_row(&mut self, ui: &mut Ui, w: &mut Wallpaper) {
        let p = self.palette;
        let pad_w = self.pad_width(ui.ctx(), ui.available_width() - 56.0 - TITLE_ROOM - PAD_GRID_GAP - GRID_W);
        let moved = kit::tall_row(ui, &p, "Position", Some("Drag the picture"), true, PAD_H, pad_w + PAD_GRID_GAP + GRID_W, |ui| {
            // Right to left: the squares, then the pad.
            let (grid, _) = ui.allocate_exact_size(vec2(GRID_W, GRID_W), Sense::hover());
            let spot = wallpaper::place_grid(ui, p.accent, grid, w);
            ui.add_space((PAD_GRID_GAP - ui.spacing().item_spacing.x).max(0.0));
            let dragged = self.wallpaper_pad(ui, w, pad_w);
            spot.or(dragged)
        });
        if let Some((x, y)) = moved {
            w.at_x = x;
            w.at_y = y;
        }
    }

    /// A wallpaper slider's row: the line, with a tick under its default (a double-click goes
    /// back to it), then its number in a box to type in. Returns the line's id while it's held.
    fn slide_row(&mut self, ui: &mut Ui, w: &mut Wallpaper, s: Slide, title: &str, range: RangeInclusive<f32>) -> Option<Id> {
        let p = self.palette;
        let standard = wallpaper::slide_default(s);
        kit::row(ui, &p, title, None, false, true, SLIDER_W + VALUE_GAP + VALUE_W, |ui| {
            // Right to left: the number, then the slider.
            slide_number(ui, &p, Id::new(("wallpaper-number", title)), s, w);
            ui.add_space((VALUE_GAP - ui.spacing().item_spacing.x).max(0.0));
            let mut slid = wallpaper::slide_value(s, w);
            let r = kit::tick_slider(ui, &p, &mut slid, range, SLIDER_W, true, standard);
            if r.double_clicked() {
                wallpaper::set_slide(s, w, standard);
            } else if r.changed() {
                wallpaper::set_slide(s, w, wallpaper::notched(s, slid, SLIDER_W, standard));
            }
            r.is_pointer_button_down_on().then_some(r.id)
        })
    }

    fn accent_swatches(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let mut chosen: Option<[u8; 3]> = None;
        ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
        let mut custom = self.prefs.accent;
        if egui::color_picker::color_edit_button_srgb(ui, &mut custom).on_hover_text("Pick any colour").changed() {
            chosen = Some(custom);
        }
        for (rgb, name) in ACCENT_PRESETS.iter().rev() {
            let (rect, resp) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::click());
            let color = Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
            let r = if resp.hovered() { 9.5 } else { 8.5 };
            ui.painter().circle_filled(rect.center(), r, color);
            if self.prefs.accent == *rgb {
                ui.painter().circle_stroke(rect.center(), 10.5, Stroke::new(1.5, p.text_strong));
            }
            if resp.on_hover_text(*name).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                chosen = Some(*rgb);
            }
        }
        if let Some(rgb) = chosen.filter(|c| *c != self.prefs.accent) {
            self.prefs.accent = rgb;
            self.restyle(ui.ctx());
            self.prefs_changed();
        }
    }

    /// Background, panel, text and hover colours over the dark/light theme, each resettable.
    pub(super) fn custom_colors_editor(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let any = self.prefs.custom_colors.as_ref().is_some_and(has_custom_colors);
        let mut colors = self.prefs.custom_colors.clone().unwrap_or_default();
        let mut changed = false;
        Grid::new("custom_colors_grid").num_columns(3).spacing([12.0, 8.0]).show(ui, |ui| {
            for (label, slot, shown) in [
                ("Background", &mut colors.background, p.main_bg),
                ("Panels", &mut colors.panel, p.sidebar_bg),
                ("Text", &mut colors.text, p.text),
                ("Secondary text", &mut colors.text_weak, p.text_weak),
                ("Hover", &mut colors.hover, p.hover_bg),
            ] {
                ui.label(RichText::new(label).color(p.text));
                let mut rgb = slot.unwrap_or([shown.r(), shown.g(), shown.b()]);
                if egui::color_picker::color_edit_button_srgb(ui, &mut rgb).changed() {
                    *slot = Some(rgb);
                    changed = true;
                }
                let reset = ui.add_enabled(slot.is_some(), Button::new("Reset").small()).on_hover_text("Back to the theme's colour");
                if reset.clicked() {
                    *slot = None;
                    changed = true;
                }
                ui.end_row();
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if kit::button_enabled(ui, &p, "Reset all", kit::Kind::Secondary, any).clicked() {
                colors = CustomColors::default();
                changed = true;
            }
            ui.label(RichText::new("Text is adjusted when it would be hard to read.").size(11.5).color(p.text_weak));
        });
        if changed {
            self.prefs.custom_colors = has_custom_colors(&colors).then_some(colors);
            self.restyle(ui.ctx());
            self.prefs_changed();
        }
    }

}

/// Slider `s`'s number in a box to type in: the slider moves as a number is typed, and a box left
/// with no number in it puts back what it was (as Glint's). That's kept for each box, as typing
/// can go straight from one box to another.
fn slide_number(ui: &mut Ui, p: &Palette, id: Id, s: Slide, w: &mut Wallpaper) {
    let v = wallpaper::slide_value(s, w);
    let typed = kit::number_box(ui, p, id, &wallpaper::slide_label(s, v), VALUE_W, true);
    let before = id.with("before");
    if typed.started {
        ui.data_mut(|d| d.insert_temp(before, v));
    }
    // The number moves the slider as it's typed (once it is a number).
    if let Some(n) = typed.changed.as_deref().and_then(|t| wallpaper::typed_number(s, t)) {
        wallpaper::set_slide(s, w, n);
    }
    if let Some(t) = typed.ended {
        // Left without a number in it: back to what it was.
        let was = ui.data_mut(|d| d.remove_temp::<f32>(before));
        if let Some(was) = was.filter(|_| wallpaper::typed_number(s, &t).is_none()) {
            wallpaper::set_slide(s, w, was);
        }
    }
}

/// The wallpaper's name in a box `width` wide, cut short with "…" (its whole path on hover); "None"
/// in grey while there is none.
fn name_box(ui: &mut Ui, width: f32, path: &str) {
    let p = crate::ui::theme::palette(ui.ctx());
    let (rect, resp) = ui.allocate_exact_size(vec2(width, kit::BUTTON_HEIGHT), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(kit::RADIUS), p.field_bg);
    painter.rect_stroke(rect, CornerRadius::same(kit::RADIUS), Stroke::new(1.0, p.field_border), StrokeKind::Inside);
    let (name, color) = match Path::new(path).file_name() {
        _ if path.is_empty() => ("None".to_owned(), p.text_weak),
        Some(name) => (name.to_string_lossy().into_owned(), p.text),
        None => (path.to_owned(), p.text),
    };
    let mut job = LayoutJob::single_section(name, TextFormat::simple(regular(12.0), color));
    job.wrap = TextWrapping::truncate_at_width((width - 16.0).max(10.0));
    let galley = painter.layout_job(job);
    painter.galley(pos2(rect.left() + 8.0, rect.center().y - galley.size().y / 2.0), galley, color);
    if !path.is_empty() {
        let _ = resp.on_hover_text(path);
    }
}

#[cfg(test)]
mod tests {
    use super::kit::headless::{key, pass, press, release, text};
    use super::*;
    use crate::config::ThemeMode;
    use egui::{Key, Pos2, Rect, UiBuilder};

    /// Inside the Opacity box (above) and the Zoom box (below it), and away from both.
    const OPACITY: Pos2 = pos2(40.0, 34.0);
    const ZOOM: Pos2 = pos2(40.0, 94.0);
    const AWAY: Pos2 = pos2(400.0, 300.0);

    /// One pass of the two boxes.
    fn boxes(ctx: &egui::Context, events: Vec<egui::Event>, w: &mut Wallpaper) {
        let p = Palette::build(ThemeMode::Dark, Color32::RED, None);
        pass(ctx, events, |ui| {
            for (i, s) in [Slide::Opacity, Slide::Zoom].into_iter().enumerate() {
                let at = Rect::from_min_size(pos2(20.0, 20.0 + 60.0 * i as f32), vec2(VALUE_W, 40.0));
                ui.scope_builder(UiBuilder::new().max_rect(at), |ui| slide_number(ui, &p, Id::new(("box", i)), s, w));
            }
        });
    }

    fn click(ctx: &egui::Context, at: Pos2, w: &mut Wallpaper) {
        boxes(ctx, press(at), w);
        boxes(ctx, release(at), w);
        // egui says a box lost the focus on this pass too.
        boxes(ctx, Vec::new(), w);
    }

    #[test]
    fn a_box_left_with_no_number_puts_it_back_also_going_straight_to_another_box() {
        let ctx = egui::Context::default();
        let mut w = Wallpaper::default();
        boxes(&ctx, Vec::new(), &mut w);
        assert_eq!((w.strength, w.zoom), (40, 0));
        // 7 typed in Opacity shows at once; wiped out, then straight on to the box below: 40 again.
        click(&ctx, OPACITY, &mut w);
        boxes(&ctx, text("7"), &mut w);
        assert_eq!(w.strength, 7);
        boxes(&ctx, key(Key::Backspace), &mut w);
        click(&ctx, ZOOM, &mut w);
        assert_eq!(w.strength, 40, "Opacity left empty for the box below");
        // The same in Zoom, then straight on to the box above.
        boxes(&ctx, text("3"), &mut w);
        assert_eq!(w.zoom, 3);
        boxes(&ctx, key(Key::Backspace), &mut w);
        click(&ctx, OPACITY, &mut w);
        assert_eq!(w.zoom, 0, "Zoom left empty for the box above");
        // A number typed stays; a box left as it was changes nothing.
        boxes(&ctx, text("55"), &mut w);
        click(&ctx, ZOOM, &mut w);
        click(&ctx, AWAY, &mut w);
        assert_eq!((w.strength, w.zoom), (55, 0));
        // Not a number, left with Enter: back too.
        click(&ctx, OPACITY, &mut w);
        boxes(&ctx, text("abc"), &mut w);
        boxes(&ctx, key(Key::Enter), &mut w);
        boxes(&ctx, Vec::new(), &mut w);
        assert_eq!(w.strength, 55);
    }
}
