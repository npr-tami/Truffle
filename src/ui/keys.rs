//! The keyboard shortcuts: one list, shown in the shortcuts window (Ctrl+H) and on Settings > Shortcuts,
//! and the note that pops up above the player bar (what the old status line said: "Link copied",
//! with Undo when there is one). Only simple keys: one key, or Ctrl and one key (Alt only for back
//! and forward, Shift only for picking songs), none of those that graphics card overlays,
//! recorders, Game Bar, Discord, Steam or PowerToys take for the whole system; and each is taken
//! with exactly its modifiers ([`take`]).

use egui::{Align2, Color32, CornerRadius, CursorIcon, Id, Key, Modifiers, Order, Rect, Sense, Shadow, Stroke, StrokeKind, Ui, pos2, vec2};

use super::fin::{self, Look};
use super::motion;
use super::theme::{bold, medium, regular};
use super::tracks::one_line;
use super::{STATUS_TOAST_TIME, TruffleApp};

/// Every shortcut, by group: the keys, and what they do.
pub(super) const GROUPS: &[(&str, &[(&str, &str)])] = &[
    (
        "Playback",
        &[
            ("Space", "Play or pause"),
            ("Ctrl →", "Next song"),
            ("Ctrl ←", "Previous song"),
            ("Ctrl ↑  Ctrl ↓", "Volume up or down"),
            ("R", "Reshuffle what's still to come"),
        ],
    ),
    (
        "The selected song (or the playing one)",
        &[
            ("Ctrl S", "Like or unlike"),
            ("Ctrl Q", "Add to queue"),
            ("Ctrl N", "Play next"),
            ("Ctrl Del", "Block or unblock"),
            ("Ctrl C", "Copy its link"),
        ],
    ),
    (
        "Picking songs in a list",
        &[
            ("Ctrl A", "Pick every song"),
            ("Ctrl Click", "Pick or drop one more"),
            ("Shift Click", "Pick a run of songs"),
            ("↑  ↓", "Move the selection"),
            ("Shift ↑  Shift ↓", "Pick while moving"),
            ("Enter", "Play the selected song"),
            ("Esc", "Let go of the picks"),
        ],
    ),
    (
        "Find",
        &[("Ctrl K", "Search Spotify"), ("Ctrl F", "Filter the songs of a list"), ("Ctrl L", "Filter your playlists"), ("Esc", "Clear the box you are in")],
    ),
    (
        "Go to",
        &[
            ("Home", "Home"),
            ("Ctrl 1  2  3", "Home, Albums, Artists"),
            ("Alt ←  Alt →", "Back, forward"),
            ("Ctrl J", "The playing song"),
            ("←  →", "Turn the covers on Home"),
        ],
    ),
    (
        "Window",
        &[
            ("Ctrl B", "Sidebar"),
            ("Ctrl P", "Side panel"),
            ("Ctrl D", "Record on the side panel"),
            ("Ctrl M", "Mini player"),
            ("Ctrl I", "Now playing view"),
            ("F11", "Fullscreen, in Now playing"),
            ("Ctrl +  −  0", "Interface size"),
            ("Ctrl ,", "Settings"),
            ("Ctrl W", "Close Settings"),
            ("Ctrl H", "These shortcuts"),
            ("Ctrl Z", "Undo"),
        ],
    ),
];

/// Takes a press of `key` (or its repeat) with exactly `modifiers` held. egui's `consume_key` lets
/// extra Shift and Alt through: Ctrl+M would also be Discord's Ctrl+Shift+M and NVIDIA's
/// Ctrl+Alt+M, R AMD's and NVIDIA's Alt+R, and AltGr (Ctrl and Alt to Windows) would set off Ctrl
/// keys.
pub(super) fn take(i: &mut egui::InputState, modifiers: Modifiers, key: Key) -> bool {
    let before = i.events.len();
    i.events.retain(|e| !pressed_with(e, modifiers, key));
    i.events.len() < before
}

/// Whether `key` was pressed with exactly `modifiers` held, left there for others too.
pub(super) fn pressed(i: &egui::InputState, modifiers: Modifiers, key: Key) -> bool {
    i.events.iter().any(|e| pressed_with(e, modifiers, key))
}

fn pressed_with(e: &egui::Event, modifiers: Modifiers, key: Key) -> bool {
    matches!(e, egui::Event::Key { key: k, modifiers: m, pressed: true, .. } if *k == key && m.matches_exact(modifiers))
}

/// The shortcuts window's size, its rows and the space under a group.
const HELP_W: f32 = 640.0;
const ROW_H: f32 = 26.0;
const GROUP_HEAD: f32 = 26.0;
const GROUP_GAP: f32 = 14.0;

/// A key chip (`.srch .k`, a little bigger), right-aligned at `right`. Returns its rect.
pub(super) fn key_chip_at(ui: &Ui, keys: &str, right: f32, cy: f32) -> Rect {
    let p = crate::ui::theme::palette(ui.ctx());
    let g = ui.painter().layout_no_wrap(keys.to_owned(), medium(11.0), p.text);
    let rect = Rect::from_min_size(pos2(right - g.size().x - 14.0, cy - (g.size().y + 8.0) / 2.0), g.size() + vec2(14.0, 8.0));
    ui.painter().rect_filled(rect, CornerRadius::same(4), p.field_bg);
    ui.painter().rect_stroke(rect, CornerRadius::same(4), Stroke::new(1.0, p.border), StrokeKind::Inside);
    ui.painter().galley(rect.center() - g.size() / 2.0, g, p.text);
    rect
}

/// A group's rows from `top`, `x0..x1` wide: its name in small capitals, then what each key does on
/// the left and the keys on the right. Returns the bottom.
pub(super) fn paint_group(ui: &Ui, x0: f32, x1: f32, top: f32, name: &str, rows: &[(&str, &str)]) -> f32 {
    let p = crate::ui::theme::palette(ui.ctx());
    let g = fin::spaced(ui, &name.to_uppercase(), bold(10.0), p.text_weak, 0.8);
    ui.painter().galley(pos2(x0, top + GROUP_HEAD / 2.0 - g.size().y / 2.0), g, p.text_weak);
    let mut y = top + GROUP_HEAD;
    for (keys, what) in rows {
        let cy = y + ROW_H / 2.0;
        let chip = key_chip_at(ui, keys, x1, cy);
        let label = one_line(ui, what, regular(12.0), p.text, chip.left() - 10.0 - x0, false);
        ui.painter().galley(pos2(x0, cy - label.size().y / 2.0), label, p.text);
        y += ROW_H;
    }
    y
}

/// The height of a column of groups.
fn column_h(groups: &[(&str, &[(&str, &str)])]) -> f32 {
    groups.iter().map(|(_, rows)| GROUP_HEAD + rows.len() as f32 * ROW_H).sum::<f32>() + GROUP_GAP * (groups.len().saturating_sub(1)) as f32
}

impl TruffleApp {
    /// Ctrl+H: the shortcuts window opens, or closes again. Opening it takes the keys from a text
    /// box being typed in and closes a song menu.
    pub(super) fn toggle_help(&mut self, ctx: &egui::Context) {
        self.help_open = !self.help_open;
        if self.help_open {
            if let Some(id) = ctx.memory(|m| m.focused()) {
                ctx.memory_mut(|m| m.surrender_focus(id));
            }
            self.song_menu = None;
            self.frame.side_over = false;
        }
    }

    /// The shortcuts window, over everything with the rest dimmed: two columns of groups (one when
    /// narrow). Esc, Ctrl+H, the × or a click beside it closes it.
    pub(super) fn help_ui(&mut self, ctx: &egui::Context) {
        if !self.help_open {
            return;
        }
        let p = self.palette;
        let screen = ctx.content_rect();
        let w = HELP_W.min(screen.width() - 32.0);
        let two = w >= 560.0;
        let (left_groups, right_groups) = if two { GROUPS.split_at(3) } else { (GROUPS, &GROUPS[..0]) };
        let body_h = column_h(left_groups).max(column_h(right_groups));
        let h = (52.0 + body_h + 16.0 + 30.0).min(screen.height() - 32.0);
        let panel = Rect::from_center_size(screen.center(), vec2(w, h));
        let mut close = false;
        let id = Id::new("help-window");
        let dim = motion::backdrop(ctx, id, Color32::from_black_alpha(150));
        let shown = egui::Area::new(id).order(Order::Foreground).fixed_pos(screen.min).fade_in(false).interactable(true).show(ctx, |ui| {
            let (_, shade) = ui.allocate_exact_size(screen.size(), Sense::click());
            ui.painter().rect_filled(screen, CornerRadius::ZERO, dim);
            if shade.clicked() && !shade.interact_pointer_pos().is_some_and(|pt| panel.contains(pt)) {
                close = true;
            }
            // It fades and grows in like the other dialogs.
            motion::dialog_box(ui, id, |ui| {
                // The window: `--panel`, a 1 px line, round 12, a deep shadow.
                ui.painter().add(Shadow { offset: [0, 18], blur: 48, spread: 0, color: Color32::from_black_alpha(170) }.as_shape(panel, CornerRadius::same(12)));
                ui.painter().rect_filled(panel, CornerRadius::same(12), p.raised_bg);
                ui.painter().rect_stroke(panel, CornerRadius::same(12), Stroke::new(1.0, p.border), StrokeKind::Inside);
                let _ = ui.interact(panel, Id::new("help-body"), Sense::click());
                ui.painter().text(pos2(panel.left() + 22.0, panel.top() + 26.0), Align2::LEFT_CENTER, "Keyboard shortcuts", bold(15.0), p.text_strong);
                let x_btn = Rect::from_min_size(pos2(panel.right() - 12.0 - 30.0, panel.top() + 11.0), vec2(30.0, 30.0));
                if fin::ib(ui, &p, x_btn, Id::new("help-close"), "x", Look::Plain, "Close (Esc)").clicked() {
                    close = true;
                }
                ui.painter().hline((panel.left() + 1.0)..=(panel.right() - 1.0), panel.top() + 52.0, Stroke::new(1.0, p.border));
                // The groups scroll when the window is too short for them all.
                let area = Rect::from_min_max(pos2(panel.left(), panel.top() + 53.0), pos2(panel.right(), panel.bottom() - 30.0));
                let mut body = ui.new_child(egui::UiBuilder::new().max_rect(area));
                body.set_clip_rect(area);
                let col_w = if two { (w - 22.0 * 2.0 - 32.0) / 2.0 } else { w - 44.0 };
                egui::ScrollArea::vertical().id_salt("help-scroll").auto_shrink(false).show(&mut body, |ui| {
                    let top = ui.cursor().top() + 10.0;
                    let mut bottom = top;
                    for (k, groups) in [left_groups, right_groups].into_iter().enumerate() {
                        let x0 = panel.left() + 22.0 + k as f32 * (col_w + 32.0);
                        let mut y = top;
                        for (name, rows) in groups {
                            y = paint_group(ui, x0, x0 + col_w, y, name, rows) + GROUP_GAP;
                        }
                        bottom = bottom.max(y);
                    }
                    ui.allocate_space(vec2(area.width() - 12.0, bottom - ui.cursor().top()));
                });
                ui.painter().text(pos2(panel.center().x, panel.bottom() - 16.0), Align2::CENTER_CENTER, "Ctrl+H or Esc closes this", regular(11.0), p.text_weak);
                (panel, ())
            });
        });
        motion::fade_out_later(ctx, shown.response.layer_id, id, egui::Vec2::ZERO);
        if close {
            self.help_open = false;
        }
    }

    /// The newest note (errors first) as a small pop-up above the player bar, centred on `cx` with its
    /// foot at `bottom`: an accent dot, the words, Undo when it can be undone, × on an error. Notes go
    /// by themselves after a few seconds; errors stay until closed.
    pub(super) fn note_popup(&mut self, ctx: &egui::Context, cx: f32, bottom: f32) {
        let now = std::time::Instant::now();
        self.toasts.retain(|t| t.error || now.duration_since(t.created) < STATUS_TOAST_TIME);
        if let Some(left) = self.toasts.iter().filter(|t| !t.error).map(|t| STATUS_TOAST_TIME.saturating_sub(now.duration_since(t.created))).min() {
            ctx.request_repaint_after(left + std::time::Duration::from_millis(20));
        }
        let Some(i) = self.toasts.iter().rposition(|t| t.error).or(self.toasts.len().checked_sub(1)) else { return };
        if self.help_open {
            // Not over the shortcuts window.
            return;
        }
        let p = self.palette;
        let (text, error, undoable) = {
            let t = &self.toasts[i];
            (t.text.clone(), t.error, t.undo.is_some())
        };
        let mut undo = false;
        let mut dismiss = false;
        // A new note rises into place as it fades in.
        let key = Id::new(("note-popup", &text));
        let arrived = motion::arrive(ctx, key, motion::MESSAGE_IN);
        let shown = egui::Area::new(Id::new("note-popup")).order(Order::Foreground).fixed_pos(pos2(0.0, 0.0)).fade_in(false).interactable(true).show(ctx, |ui| {
            let screen = ctx.content_rect();
            let max_w = (screen.width() - 120.0).clamp(120.0, 560.0);
            let g = one_line(ui, &text, regular(12.0), p.text, max_w, false);
            let action = if undoable { Some("Undo") } else if error { Some("×") } else { None };
            let ag = action.map(|a| ui.painter().layout_no_wrap(a.to_owned(), if a == "Undo" { bold(11.5) } else { regular(14.0) }, p.accent_ink));
            let inner = 14.0 + 6.0 + 9.0 + g.size().x + ag.as_ref().map_or(0.0, |a| 16.0 + a.size().x) + 14.0;
            // Centred on `cx` (over the play button), but never past the window's edges.
            let x = (cx - inner / 2.0).clamp(screen.left() + 8.0, (screen.right() - 8.0 - inner).max(screen.left() + 8.0));
            let pill = Rect::from_min_size(pos2(x, bottom - 34.0), vec2(inner, 34.0));
            // Clicks on the pill stay on it (not on the song rows under it).
            let _ = ui.interact(pill, Id::new("note-pill"), Sense::click());
            ui.multiply_opacity(arrived);
            motion::moved(ui, vec2(0.0, motion::offset(motion::MESSAGE_RISE, arrived, motion::moves())), |ui| {
                ui.painter().add(Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(150) }.as_shape(pill, CornerRadius::same(8)));
                ui.painter().rect_filled(pill, CornerRadius::same(8), p.raised_bg);
                ui.painter().rect_stroke(pill, CornerRadius::same(8), Stroke::new(1.0, p.border), StrokeKind::Inside);
                let dot = pos2(pill.left() + 14.0 + 3.0, pill.center().y);
                ui.painter().circle_filled(dot, 3.0, if error { p.error } else { p.accent_ink });
                let tx = dot.x + 3.0 + 9.0;
                let gw = g.size().x;
                ui.painter().galley(pos2(tx, pill.center().y - g.size().y / 2.0), g, if error { p.error } else { p.text });
                if let Some(ag) = ag {
                    let r = Rect::from_min_size(pos2(tx + gw + 16.0, pill.center().y - ag.size().y / 2.0), ag.size());
                    let resp = ui.interact(r.expand(6.0), Id::new("note-action"), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
                    let color = motion::color(ctx, Id::new("note-action"), if resp.hovered() { p.text_strong } else if undoable { p.accent_ink } else { p.text_weak }, motion::HOVER);
                    ui.painter().galley_with_override_text_color(r.min, ag, color);
                    if resp.on_hover_text(if undoable { "Undo (Ctrl+Z)" } else { "Close" }).clicked() {
                        if undoable { undo = true } else { dismiss = true }
                    }
                }
            });
        });
        // Once it goes (or another note takes its place), it sinks back as it fades out.
        motion::fade_out_later(ctx, shown.response.layer_id, key, vec2(0.0, motion::MESSAGE_RISE));
        if undo {
            let t = self.toasts.remove(i);
            if let Some(u) = t.undo {
                self.apply_undo(u);
            }
        } else if dismiss {
            self.toasts.remove(i);
        }
    }

    /// Ctrl+↑ and Ctrl+↓: the volume by 5, and a note saying where it is.
    pub(super) fn nudge_volume(&mut self, by: f32) {
        self.volume_ui = (self.volume_ui + by).clamp(0.0, 100.0);
        self.volume_before_mute = None;
        let v = self.volume_ui.round() as u8;
        self.playback.volume = v;
        self.send(super::Command::SetVolume(v));
        // One volume note at a time, not a pile of them.
        self.toasts.retain(|t| !t.text.starts_with("Volume "));
        self.note(format!("Volume {v}%"), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::kit::headless::pass;

    /// Keys other programs take for the whole system, so none may be Truffle's. AMD Adrenalin's
    /// Ctrl Shift letters are three keys, which no shortcut is anyway.
    const TAKEN: &[(&str, &str)] = &[
        ("Alt R", "AMD Adrenalin's and NVIDIA App's overlays"),
        ("Alt U", "AMD Adrenalin"),
        ("Alt Z", "NVIDIA App's overlay"),
        ("Alt Shift R", "NVIDIA App"),
        ("Alt F1", "NVIDIA App"),
        ("Alt F2", "NVIDIA App"),
        ("Alt F3", "NVIDIA App"),
        ("Alt F9", "NVIDIA App"),
        ("Alt F10", "NVIDIA App"),
        ("Ctrl Alt M", "NVIDIA App"),
        ("Alt I", "Intel Graphics Software"),
        ("Alt O", "Intel Graphics Software"),
        ("Ctrl Alt F12", "Intel Graphics Software"),
        ("Ctrl Alt ←  →  ↑  ↓", "Intel Graphics Software"),
        ("Win G", "Windows Game Bar"),
        ("Win Alt R", "Windows Game Bar"),
        ("Ctrl Shift M", "Discord"),
        ("Ctrl Shift D", "Discord"),
        ("Shift Tab", "Steam's overlay"),
        ("F12", "Steam"),
        ("Alt Space", "PowerToys Run"),
    ];

    const MODIFIERS: [&str; 4] = ["Ctrl", "Alt", "Shift", "Win"];

    /// Each key of a row's keys, its modifiers in the order of MODIFIERS: "Ctrl 1  2  3" is
    /// "Ctrl 1", "Ctrl 2" and "Ctrl 3".
    fn combos(keys: &str) -> Vec<String> {
        let mut held: Vec<&str> = Vec::new();
        let mut out = Vec::new();
        for part in keys.split("  ") {
            let mut words: Vec<&str> = part.split(' ').filter(|w| !w.is_empty()).collect();
            let Some(key) = words.pop() else { continue };
            if !words.is_empty() {
                held = words;
            }
            let mut combo: Vec<&str> = MODIFIERS.iter().copied().filter(|m| held.contains(m)).collect();
            combo.extend(held.iter().copied().filter(|m| !MODIFIERS.contains(m)));
            combo.push(key);
            out.push(combo.join(" "));
        }
        out
    }

    /// Why a key breaks the rules, if it does. Simple keys only: one key, or Ctrl and a letter, a
    /// digit, an arrow, + − , Del or a click; Alt only with ← → (back and forward), Shift only with
    /// ↑ ↓ or a click (picking songs); and none that another program takes.
    fn broken(combo: &str) -> Option<String> {
        if let Some((_, who)) = TAKEN.iter().find(|(keys, _)| combos(keys).iter().any(|k| k == combo)) {
            return Some(format!("{combo} is taken by {who}"));
        }
        let words: Vec<&str> = combo.split(' ').collect();
        let (&key, mods) = words.split_last()?;
        let letter = key.len() == 1 && key.bytes().all(|b| b.is_ascii_uppercase());
        let digit = key.len() == 1 && key.bytes().all(|b| b.is_ascii_digit());
        let fkey = key.len() > 1 && key.starts_with('F') && key[1..].bytes().all(|b| b.is_ascii_digit());
        let arrow = ["←", "→", "↑", "↓"].contains(&key);
        let simple = match mods {
            [] => true,
            ["Ctrl"] => letter || digit || arrow || ["+", "−", ",", "Del", "Click"].contains(&key),
            ["Alt"] => ["←", "→"].contains(&key),
            ["Shift"] => ["↑", "↓", "Click"].contains(&key),
            _ => false,
        };
        match mods {
            _ if simple => None,
            ["Alt"] if letter || fkey => Some(format!("{combo}: Alt and a letter or F-key")),
            [_, _, ..] => Some(format!("{combo}: three keys")),
            _ => Some(format!("{combo}: not a simple key")),
        }
    }

    #[test]
    fn the_rules_catch_the_keys_they_should() {
        for fine in ["Ctrl N", "Ctrl Q", "Ctrl S", "Space", "R", "Home", "Esc", "Enter", "Ctrl 1", "Ctrl −", "Ctrl ,", "Ctrl Del", "Ctrl Click", "Shift Click", "Shift ↓", "Alt ←", "Ctrl →"] {
            assert_eq!(broken(fine), None, "{fine}");
        }
        let bad = [
            "Ctrl Shift E", "Shift Ctrl N", "Ctrl Alt N", "Alt A", "Alt R", "Alt F4", "Alt Shift R", "Alt Space", "Ctrl Alt ←", "Ctrl Shift M",
            "Win G", "Win Alt R", "F12", "Shift Tab", "Shift A", "Alt ↑", "Ctrl F5", "Ctrl Tab", "Ctrl Shift Alt X",
        ];
        for keys in bad {
            assert!(broken(&combos(keys)[0]).is_some(), "{keys}");
        }
        assert_eq!(combos("Ctrl +  −  0"), ["Ctrl +", "Ctrl −", "Ctrl 0"]);
        assert_eq!(combos("Shift Alt R"), ["Alt Shift R"]);
        assert_eq!(combos("↑  ↓"), ["↑", "↓"]);
    }

    /// The row a key is listed in: what it does.
    fn listed(combo: &str) -> Option<&'static str> {
        GROUPS.iter().flat_map(|(_, rows)| rows.iter()).find(|(keys, _)| combos(keys).iter().any(|k| k == combo)).map(|&(_, what)| what)
    }

    #[test]
    fn every_listed_key_is_simple_and_clashes_with_nothing() {
        for (group, rows) in GROUPS {
            for (keys, what) in *rows {
                for combo in combos(keys) {
                    assert_eq!(broken(&combo), None, "{group}: {keys} ({what})");
                }
            }
        }
        assert_eq!(listed("Ctrl Q"), Some("Add to queue"));
        assert_eq!(listed("Ctrl N"), Some("Play next"));
        assert_eq!(listed("Ctrl S"), Some("Like or unlike"));
        for gone in ["Ctrl E", "Ctrl Shift E", "Alt A", "Alt R"] {
            assert_eq!(listed(gone), None, "{gone}");
        }
        assert!(!GROUPS.iter().flat_map(|(_, rows)| rows.iter()).any(|(_, what)| what.contains("Quit")));
    }

    /// The code of each Rust file under src/ui (its tests left out, from the `#[cfg(test)]` of a
    /// module on), by its file name.
    fn ui_code() -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut dirs = vec![std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ui"))];
        while let Some(dir) = dirs.pop() {
            for path in std::fs::read_dir(&dir).expect("src/ui").flatten().map(|e| e.path()) {
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let text = std::fs::read_to_string(&path).expect("readable");
                    let lines: Vec<&str> = text.lines().collect();
                    let tests = lines.windows(2).position(|w| w[0] == "#[cfg(test)]" && ["mod ", "pub mod ", "pub(super) mod "].iter().any(|m| w[1].starts_with(m)));
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    out.push((name, lines[..tests.unwrap_or(lines.len())].join("\n")));
                }
            }
        }
        out
    }

    /// The keys `code` takes or looks at through `keys::take` and `keys::pressed`.
    fn handled_in(code: &str) -> Vec<String> {
        let mut out = Vec::new();
        for call in ["keys::take(", "keys::pressed("] {
            for (at, _) in code.match_indices(call) {
                let args = &code[at + call.len()..];
                let args: Vec<&str> = args[..args.find(')').expect("its end")].split(',').map(str::trim).collect();
                let &[_, mods, key] = args.as_slice() else { panic!("{call}{args:?}") };
                let mut combo: Vec<&str> = Vec::new();
                for m in mods.split('|').map(|m| m.trim().rsplit("::").next().unwrap_or(m)) {
                    match m {
                        "CTRL" | "COMMAND" => combo.push("Ctrl"),
                        "ALT" => combo.push("Alt"),
                        "SHIFT" => combo.push("Shift"),
                        "NONE" => {}
                        other => combo.push(other),
                    }
                }
                let key = key.rsplit("::").next().unwrap_or(key);
                combo.push(match key {
                    "ArrowLeft" => "←",
                    "ArrowRight" => "→",
                    "ArrowUp" => "↑",
                    "ArrowDown" => "↓",
                    "Comma" => ",",
                    "Delete" => "Del",
                    "Escape" => "Esc",
                    "Plus" => "+",
                    "Minus" => "−",
                    _ => key.strip_prefix("Num").unwrap_or(key),
                });
                out.push(combos(&combo.join(" ")).remove(0));
            }
        }
        out
    }

    #[test]
    fn every_handled_key_is_listed_and_every_listed_key_handled() {
        let listed: Vec<String> = GROUPS.iter().flat_map(|(_, rows)| rows.iter().flat_map(|(keys, _)| combos(keys))).collect();
        let mut handled: Vec<(String, String)> = Vec::new();
        for (file, code) in ui_code() {
            // egui's own matching lets extra Shift and Alt through. Only the size keys use it: Ctrl +
            // is Ctrl Shift = on many keyboards.
            assert!(!code.contains("consume_key("), "{file} takes a key loosely: use keys::take");
            assert!(!code.contains("consume_shortcut(") || file == "screens.rs", "{file} takes a key loosely: use keys::take");
            // Text boxes and dialogs see Enter and Esc as they come.
            for (at, _) in code.match_indices("key_pressed(") {
                let arg = &code[at + "key_pressed(".len()..];
                let arg = &arg[..arg.find(')').expect("its end")];
                assert!(arg == "key" || arg.ends_with("Key::Enter") || arg.ends_with("Key::Escape"), "{file}: key_pressed({arg})");
            }
            for combo in handled_in(&code) {
                assert_eq!(broken(&combo), None, "{file}");
                assert!(listed.contains(&combo), "{file} takes {combo}, which the shortcuts list doesn't show");
                handled.push((combo, file.clone()));
            }
        }
        assert!(handled.len() > 30, "only {} keys found: the check can't read the code any more", handled.len());
        // Taken another way: egui's copy, clicks, and the size keys.
        let elsewhere = ["Ctrl C", "Ctrl Click", "Shift Click", "Ctrl +", "Ctrl −", "Ctrl 0"];
        for combo in &listed {
            assert!(handled.iter().any(|(c, _)| c == combo) || elsewhere.contains(&combo.as_str()), "{combo} is listed, but nothing takes it");
        }
        // Ctrl Q is only the song keys' (Add to queue): nothing quits on it any more.
        let q: Vec<&str> = handled.iter().filter(|(c, _)| c == "Ctrl Q").map(|(_, f)| f.as_str()).collect();
        assert_eq!(q, ["songmenu.rs"]);
    }

    /// Whether `take` has a press of `key` with `held` when asked for it with `wanted`, and
    /// whether it was left for others then.
    fn takes(wanted: Modifiers, key: Key, held: Modifiers) -> (bool, bool) {
        let ctx = egui::Context::default();
        let event = egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: held };
        let mut out = (false, false);
        pass(&ctx, vec![event], |ui| {
            out.0 = ui.input_mut(|i| take(i, wanted, key));
            out.1 = ui.input(|i| pressed(i, held, key));
        });
        out
    }

    #[test]
    fn keys_are_taken_with_exactly_their_modifiers() {
        // Ctrl as Windows sends it (Ctrl and Command), and AltGr (Ctrl and Alt).
        let ctrl = Modifiers { ctrl: true, command: true, ..Modifiers::NONE };
        let altgr = Modifiers { alt: true, ..ctrl };
        let ctrl_shift = Modifiers { shift: true, ..ctrl };
        assert_eq!(takes(Modifiers::CTRL, Key::M, ctrl), (true, false));
        assert_eq!(takes(Modifiers::COMMAND, Key::A, ctrl), (true, false));
        assert_eq!(takes(Modifiers::SHIFT, Key::ArrowUp, Modifiers::SHIFT), (true, false));
        // Discord's Ctrl Shift M, NVIDIA's Ctrl Alt M (and AltGr M), the overlays' Alt R, PowerToys'
        // Alt Space and Intel's Ctrl Alt → are not Truffle's Ctrl M, R, Space and Ctrl →.
        for (wanted, key, held) in [
            (Modifiers::CTRL, Key::M, ctrl_shift),
            (Modifiers::CTRL, Key::M, altgr),
            (Modifiers::NONE, Key::R, Modifiers::ALT),
            (Modifiers::NONE, Key::R, Modifiers { shift: true, ..Modifiers::ALT }),
            (Modifiers::NONE, Key::Space, Modifiers::ALT),
            (Modifiers::CTRL, Key::ArrowRight, altgr),
            (Modifiers::CTRL, Key::N, ctrl_shift),
            (Modifiers::NONE, Key::ArrowUp, Modifiers::SHIFT),
            (Modifiers::NONE, Key::Q, ctrl),
            (Modifiers::CTRL, Key::Q, Modifiers::NONE),
        ] {
            assert_eq!(takes(wanted, key, held), (false, true), "{key:?} with {held:?}");
        }
    }
}
