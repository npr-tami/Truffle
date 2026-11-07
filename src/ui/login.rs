//! Startup screen, shown until the controller reports a logged-in session: the logo, the name and
//! the tagline, then the login button or what it is doing meanwhile.

use std::time::{Duration, Instant};

use egui::{ColorImage, CornerRadius, CursorIcon, Frame, Image, Rect, RichText, Sense, TextureHandle, Ui, UiBuilder, pos2, vec2};

use super::fin;
use super::theme::{bold, regular};
use super::wallpaper::TEXTURE;
use crate::icon::IconStyle;
use super::{AuthState, TruffleApp};
use crate::model::Command;

/// After this long in "logging in", offer a way back to the login button.
const LOGIN_ESCAPE_AFTER: Duration = Duration::from_secs(8);

const TAGLINE: &str = "Shuffle that's actually random.";
/// Shown while it starts and logs in by itself (a browser login says what to do instead).
const WARMING_UP: &str = "Warming up the turntable…";
/// The logo's size on screen, in points.
const LOGO: f32 = 112.0;
/// The wordmark's width on screen, in points (its height follows).
const WORDMARK_W: f32 = 270.0;

/// The chrome "TRUFFLE" wordmark, drawn at 3x (`assets/wordmark.svg`): one for dark backgrounds,
/// one for light ones (a darker edge, and a blue sparkle that stays visible on white).
pub(super) static WORDMARK: &[u8] = include_bytes!("../../assets/wordmark.png");
pub(super) static WORDMARK_LIGHT: &[u8] = include_bytes!("../../assets/wordmark_light.png");

/// The startup screen's pictures, as textures: the logo in the app icon's style, and the
/// wordmark.
pub(super) struct Brand {
    style: IconStyle,
    logo: TextureHandle,
    wordmark: TextureHandle,
    wordmark_light: TextureHandle,
}

impl Brand {
    fn load(ctx: &egui::Context, style: IconStyle) -> Self {
        let icon = crate::icon::window_icon(style);
        let logo = rgba_image([icon.width as usize, icon.height as usize], &icon.rgba);
        Self {
            style,
            logo: ctx.load_texture("truffle-logo", logo, TEXTURE),
            wordmark: ctx.load_texture("truffle-wordmark", png_image(WORDMARK), TEXTURE),
            wordmark_light: ctx.load_texture("truffle-wordmark-light", png_image(WORDMARK_LIGHT), TEXTURE),
        }
    }
}

impl TruffleApp {
    pub(super) fn login_screen(&mut self, ui: &mut Ui) {
        let p = self.palette;
        egui::CentralPanel::default().frame(Frame::new().fill(p.main_bg)).show(ui, |ui| {
            let full = ui.max_rect();
            // A bare title bar: it drags the window and has the window buttons.
            let bar = Rect::from_min_size(full.min, vec2(full.width(), 40.0));
            super::frame::window_controls(ui, bar, full.width() < super::frame::NARROW, !p.dark);
            let body = Rect::from_min_max(pos2(full.left(), bar.bottom()), full.max);
            ui.scope_builder(UiBuilder::new().max_rect(body), |ui| self.login_contents(ui));
        });
        self.window_edges(ui.ctx());
    }

    fn login_contents(&mut self, ui: &mut Ui) {
        let p = self.palette;
        let (text, muted, faint) = (p.text, p.text_weak, p.text_faint);
        let content_h = 360.0;
        let top = ((ui.available_height() - content_h) / 2.0 - 20.0).max(16.0);
        ui.add_space(top);
        ui.vertical_centered(|ui| {
            let style = self.shell.icon_shown.unwrap_or(IconStyle::Silver);
            if self.brand.as_ref().is_none_or(|b| b.style != style) {
                self.brand = Some(Brand::load(ui.ctx(), style));
            }
            let Some(brand) = self.brand.as_ref() else { return };
            ui.add(Image::new(&brand.logo).fit_to_exact_size(vec2(LOGO, LOGO)));
            ui.add_space(12.0);
            let wordmark = if p.dark { &brand.wordmark } else { &brand.wordmark_light };
            let [w, h] = wordmark.size();
            ui.add(Image::new(wordmark).fit_to_exact_size(vec2(WORDMARK_W, WORDMARK_W * h as f32 / w.max(1) as f32)))
                .on_hover_text("Truffle");
            ui.add_space(8.0);
            ui.label(RichText::new(TAGLINE).font(regular(14.0)).color(muted));
            ui.add_space(30.0);

            match self.auth {
                AuthState::Starting => {
                    ui.add(egui::Spinner::new().size(24.0).color(p.accent_ink));
                    ui.add_space(10.0);
                    ui.label(RichText::new(WARMING_UP).font(regular(13.0)).color(text));
                }
                AuthState::LoggingIn => {
                    ui.add(egui::Spinner::new().size(24.0).color(p.accent_ink));
                    ui.add_space(10.0);
                    let status = if self.interactive_login { "Finish logging in in your browser" } else { WARMING_UP };
                    ui.label(RichText::new(status).font(regular(13.0)).color(text));
                    if self.interactive_login {
                        ui.add_space(2.0);
                        ui.label(RichText::new("Come back here when it says you're done.").font(regular(12.0)).color(muted));
                    }
                    let waited = self.login_started.map_or(Duration::ZERO, |t| t.elapsed());
                    if self.interactive_login {
                        if waited >= LOGIN_ESCAPE_AFTER {
                            ui.add_space(12.0);
                            if ui.link(RichText::new("Start over").font(regular(12.0))).clicked() {
                                self.auth = AuthState::NeedLogin;
                                self.interactive_login = false;
                            }
                        } else {
                            ui.ctx().request_repaint_after(LOGIN_ESCAPE_AFTER - waited);
                        }
                    }
                }
                AuthState::NeedLogin | AuthState::LoggedIn => {
                    if login_button(ui, &p, "Log in with Spotify").clicked() {
                        self.auth = AuthState::LoggingIn;
                        self.interactive_login = true;
                        self.login_started = Some(Instant::now());
                        self.send(Command::Login { interactive: true });
                    }
                    ui.add_space(10.0);
                    ui.label(RichText::new("Opens Spotify in your browser.").font(regular(12.0)).color(muted));
                }
            }

            ui.add_space(28.0);
            ui.label(RichText::new("Free accounts play songs from YouTube").font(regular(11.5)).color(faint));
        });
    }
}

/// The login button: the accent, 40 tall, round 8, 12 px semibold capitals spaced out.
fn login_button(ui: &mut Ui, p: &super::theme::Palette, text: &str) -> egui::Response {
    let g = fin::spaced(ui, &text.to_uppercase(), bold(12.0), p.on_accent, 0.7);
    let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 56.0, 40.0), Sense::click());
    let fill = if resp.hovered() { fin::brighten(p.accent, 1.07) } else { p.accent };
    ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
    ui.painter().galley(rect.center() - g.size() / 2.0, g, p.on_accent);
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// Straight-alpha RGBA pixels as an image (empty if they don't fit the size). The textures are
/// mipmapped, so the large artwork stays smooth at the screen's smaller size.
fn rgba_image(size: [usize; 2], rgba: &[u8]) -> ColorImage {
    if rgba.len() == size[0] * size[1] * 4 && !rgba.is_empty() {
        ColorImage::from_rgba_unmultiplied(size, rgba)
    } else {
        ColorImage::filled([1, 1], egui::Color32::TRANSPARENT)
    }
}

/// A mipmapped texture from embedded PNG bytes.
pub(super) fn png_texture(ctx: &egui::Context, name: &str, bytes: &[u8]) -> TextureHandle {
    ctx.load_texture(name, png_image(bytes), TEXTURE)
}

fn png_image(bytes: &[u8]) -> ColorImage {
    match image::load_from_memory_with_format(bytes, image::ImageFormat::Png) {
        Ok(img) => {
            let img = img.into_rgba8();
            rgba_image([img.width() as usize, img.height() as usize], img.as_raw())
        }
        Err(e) => {
            log::warn!("could not decode the wordmark: {e}");
            rgba_image([0, 0], &[])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wordmarks_decode() {
        for bytes in [WORDMARK, WORDMARK_LIGHT] {
            let img = png_image(bytes);
            let [w, h] = img.size;
            assert!(w > 1000 && h > 200 && w > 3 * h, "{w}x{h}");
            // Trimmed to the drawing, with a see-through margin.
            assert_eq!(img.pixels[0].a(), 0);
        }
    }
}
