//! Notification-area (tray) icon with a small playback menu. Must live on the UI thread (the
//! thread running the window's message loop); clicks arrive through tray-icon's global handlers.

use std::sync::Once;

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use super::state::{NowPlaying, Status, now_playing_label, tray_tooltip};
use super::{Internal, PlatformAction, forward};
use crate::icon::IconStyle;
use crate::model::Command;

const ID_NOW_PLAYING: &str = "truffle.now_playing";
const ID_PLAY_PAUSE: &str = "truffle.play_pause";
const ID_NEXT: &str = "truffle.next";
const ID_PREVIOUS: &str = "truffle.previous";
const ID_SHOW: &str = "truffle.show";
const ID_MINI: &str = "truffle.mini_player";
const ID_QUIT: &str = "truffle.quit";

/// What a tray menu item does.
pub fn menu_action(id: &str) -> Option<PlatformAction> {
    Some(match id {
        ID_PLAY_PAUSE => PlatformAction::Command(Command::PlayPause),
        ID_NEXT => PlatformAction::Command(Command::Next),
        ID_PREVIOUS => PlatformAction::Command(Command::Previous),
        ID_SHOW => PlatformAction::ShowWindow,
        ID_MINI => PlatformAction::ToggleMiniPlayer,
        ID_QUIT => PlatformAction::Quit,
        _ => return None,
    })
}

/// tray-icon's handlers are process-wide and can be set only once; they forward into whatever
/// `Platform` is current (see `super::forward`).
fn install_global_handlers() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        TrayIconEvent::set_event_handler(Some(|event: TrayIconEvent| {
            let show = matches!(
                event,
                TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
                    | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. }
            );
            if show {
                forward(Internal::Action(PlatformAction::ShowWindow));
            }
        }));
        MenuEvent::set_event_handler(Some(|event: MenuEvent| {
            if let Some(action) = menu_action(event.id().as_ref()) {
                forward(Internal::Action(action));
            }
        }));
    });
}

/// The tray icon in `style`, rendered for the notification area's size (sharper than a scaled
/// bitmap).
fn tray_image(style: IconStyle) -> Result<tray_icon::Icon, String> {
    #[cfg(windows)]
    let size = small_icon_size();
    #[cfg(not(windows))]
    let size = 32;
    let (rgba, width, height) = crate::icon::tray_icon_rgba(style, size);
    tray_icon::Icon::from_rgba(rgba, width, height).map_err(|e| format!("bad tray icon image: {e}"))
}

/// The size Windows draws small icons (tray) at: 16 px at 100 % scaling, 20 at 125 %, 24 at 150 %.
#[cfg(windows)]
fn small_icon_size() -> u32 {
    const SM_CXSMICON: i32 = 49;
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetSystemMetrics(index: i32) -> i32;
    }
    // SAFETY: no pointers involved.
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) };
    u32::try_from(size).ok().filter(|s| (8..=256).contains(s)).unwrap_or(16)
}

pub struct Tray {
    icon: TrayIcon,
    now_playing: MenuItem,
    play_pause: MenuItem,
    shown_label: String,
    shown_playing: bool,
    shown_tooltip: String,
}

impl Tray {
    pub fn new(style: IconStyle) -> Result<Self, String> {
        install_global_handlers();

        let now_playing = MenuItem::with_id(ID_NOW_PLAYING, now_playing_label(None, Status::Stopped), false, None);
        let play_pause = MenuItem::with_id(ID_PLAY_PAUSE, "Play", true, None);
        let next = MenuItem::with_id(ID_NEXT, "Next", true, None);
        let previous = MenuItem::with_id(ID_PREVIOUS, "Previous", true, None);
        let show = MenuItem::with_id(ID_SHOW, "Show Truffle", true, None);
        let mini = MenuItem::with_id(ID_MINI, "Mini player", true, None);
        let quit = MenuItem::with_id(ID_QUIT, "Quit", true, None);
        let menu = Menu::new();
        menu.append_items(&[
            &now_playing,
            &play_pause,
            &next,
            &previous,
            &PredefinedMenuItem::separator(),
            &show,
            &mini,
            &PredefinedMenuItem::separator(),
            &quit,
        ])
        .map_err(|e| format!("could not build the tray menu: {e}"))?;

        let image = tray_image(style)?;
        let tooltip = tray_tooltip(None);
        let icon = TrayIconBuilder::new()
            .with_id("truffle")
            .with_icon(image)
            .with_tooltip(&tooltip)
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()
            .map_err(|e| format!("could not add the tray icon: {e}"))?;

        Ok(Self {
            icon,
            now_playing,
            play_pause,
            shown_label: now_playing_label(None, Status::Stopped),
            shown_playing: false,
            shown_tooltip: tooltip,
        })
    }

    /// Shows the icon in `style`.
    pub fn set_style(&mut self, style: IconStyle) {
        if let Err(e) = tray_image(style).and_then(|image| self.icon.set_icon(Some(image)).map_err(|e| e.to_string())) {
            log::warn!("tray: could not change the icon: {e}");
        }
    }

    /// Refresh the tooltip and menu; only touches what actually changed.
    pub fn update(&mut self, track: Option<&NowPlaying>, status: Status) {
        let label = now_playing_label(track, status);
        if label != self.shown_label {
            self.now_playing.set_text(&label);
            self.shown_label = label;
        }
        let playing = status == Status::Playing;
        if playing != self.shown_playing {
            self.play_pause.set_text(if playing { "Pause" } else { "Play" });
            self.shown_playing = playing;
        }
        let tooltip = tray_tooltip(track);
        if tooltip != self.shown_tooltip {
            // Fails only while Explorer isn't running. tray-icon then keeps the old text (and
            // re-adds the icon with it when Explorer is back), so it is tried again on the next
            // update rather than taken as shown.
            match self.icon.set_tooltip(Some(&tooltip)) {
                Ok(()) => self.shown_tooltip = tooltip,
                Err(e) => log::debug!("tray: could not set the tooltip: {e}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_ids_map_to_actions() {
        assert!(matches!(menu_action(ID_PLAY_PAUSE), Some(PlatformAction::Command(Command::PlayPause))));
        assert!(matches!(menu_action(ID_NEXT), Some(PlatformAction::Command(Command::Next))));
        assert!(matches!(menu_action(ID_PREVIOUS), Some(PlatformAction::Command(Command::Previous))));
        assert!(matches!(menu_action(ID_SHOW), Some(PlatformAction::ShowWindow)));
        assert!(matches!(menu_action(ID_MINI), Some(PlatformAction::ToggleMiniPlayer)));
        assert!(matches!(menu_action(ID_QUIT), Some(PlatformAction::Quit)));
        assert!(menu_action(ID_NOW_PLAYING).is_none());
        assert!(menu_action("something else").is_none());
    }
}
