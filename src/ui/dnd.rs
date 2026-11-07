//! Drag-and-drop payloads shared by the sidebar and the song list (egui's
//! `dnd_drag_source` / `dnd_drop_zone`).

/// Songs being dragged, e.g. from the song list onto a sleeve to add them to that playlist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SongsDrag {
    pub uris: Vec<String>,
}
