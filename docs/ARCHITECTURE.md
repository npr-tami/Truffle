# Truffle architecture

Lightweight native Windows Spotify client: Rust + librespot 0.8 (playback, library reads) +
egui/eframe 0.36 with the glow renderer (UI) + optional official Web API (editing).
Spotify Premium required.

## Threads

- **Main / UI thread** (eframe, winit message loop): `ui::TruffleApp`. Never blocks. Sends
  `model::Command` over a tokio unbounded channel; drains `model::Event` from a std mpsc channel.
  - `App::logic` runs before every frame **and also while the window is hidden in the tray or
    minimized** (eframe 0.36 then runs logic only, no egui pass, throttled). It is the only place
    that drains controller events, feeds `Platform::update`, handles `Platform::poll` actions,
    close requests and the single-instance activation flag. So media keys, tray clicks and the
    Discord status keep working with no window on screen.
  - `App::ui` only draws (skipped while hidden), and only visible frames schedule periodic
    repaints, so a hidden Truffle uses no CPU between events.
  - The tray icon and the Windows media overlay (SMTC) live on this thread. Tray clicks arrive
    inside the message loop; media-key presses on a Windows thread-pool thread. Both go through
    one channel in `platform` and wake the UI with `request_repaint`.
- **tokio runtime** (2 workers + blocking pool): `controller::run` plus everything async. The
  controller wakes the UI through `UiLink::send` (event + `request_repaint`). Disk writes, image
  decoding, the startup-entry and Start-menu jobs run on the blocking pool.
- **Last.fm**: no thread of its own. `lastfm::Scrobbler` lives inside the controller and is
  driven synchronously from its loop; each network call (one scrobble batch at a time, "now
  playing", the connect flow) is a spawned tokio task that reports back as `Job::Lastfm`.
  Opening the approval page runs on the blocking pool. See "Last.fm" below.
- **Audio**: one librespot `Player` thread per player slot (a second slot exists only while
  crossfade is on), each with its own rodio/WASAPI output stream; Windows mixes them.
- **`truffle-discord`**: the Discord IPC connection (`platform/discord.rs`). Takes the newest
  presence from a channel, debounces (700 ms) and rate-limits (5 per 20 s), waits for each reply.
  While nothing changes it re-sends the status every 30 s, so a Discord restart is noticed (the
  pipe only reports being closed when written to) and the status comes back.
  Dropping its handle stops it without waiting, so a stuck pipe never blocks the UI.
- **`truffle-activate`**: waits on a named event; a second launch signals it and the window is
  shown (`main.rs`, `single_instance`).
- Short-lived: `truffle-shortcut` (COM, creates the .lnk), `truffle-file-dialog` (cover picker),
  `truffle-font` (loads a script font), `truffle-player-shutdown`.

## Start-up and window states (`main.rs`, `ui/shell.rs`)

- **Single instance**: a named mutex (`Local\Truffle-…-instance`, `-dev` suffix in debug builds)
  plus a named event. A second launch signals the event and exits (a `--minimized` relaunch
  exits quietly); a launch while the old one is still closing waits up to 15 s for it.
- **`--minimized`** (used by the startup entry): `main.rs` minimises and hides the still-unpainted
  window before eframe's first frame when `tray_icon` is on, else just minimises it. eframe
  never shows a minimised window by itself. `shell_logic` then marks it hidden, or, if the tray
  icon could not be created, makes it visible-minimised so it is reachable from the taskbar.
- **Close to tray**: a close request is turned into hiding only when `close_to_tray` is on *and*
  `Platform::tray_available()`. Tray menu Quit sets a flag so the next close request is real.
- **Mini player**: the same root viewport, switched to 380×120, borderless, optionally always on
  top; the full window's size, position and maximized state are restored on the way back.
- Window sizes are saved in logical pixels (`inner_rect × zoom`); `ui::initial_window_size` and
  the mini-player exit share one sanitising rule (`ui/window.rs`).
- **Interface size** (`ui/screens.rs`): egui's zoom factor, per screen. `screen_sizes` keeps the
  size set on a screen by its key (monitor name and resolution, `window::monitor_of`; the
  resolution alone without it). A screen without one gets `1 + 0.75 × (height / 1080 − 1)`,
  height in Windows' units, kept within 1.0–2.5 and rounded to 0.05, with a one-time toast when
  that is above 1.0 (`big_screen_tips`). The screen is looked up before the first frame, then
  only when the window's place, egui's monitor size or Windows' scale changes. The mini player's
  size follows the zoom, so its screen is the one under its top left corner
  (`window::monitor_at`), not the one it is mostly on, which could flip back and forth. egui
  measures the window in the last pass's zoom, so in the pass where a new zoom applies its sizes
  are converted with the old one (`ScreenZoom::next_pass`). egui's own zoom keys are off: Ctrl
  +/− set the screen's size, Ctrl 0 forgets it. "Grow with the window" multiplies by
  `clamp(width / 1400, 1, 1.5)` (logical width, so the zoom can't feed back), once the width has
  stayed 0.3 s; not in the mini player. The width is read before the first frame too, so it
  opens grown. An older `ui_scale` other than 1.0 becomes the size of the first screen Truffle
  opens on, then is 1.0.

## Modules

| File | Role |
|---|---|
| `model.rs` | Types shared between modules. They grow by adding (a new variant, or a field with a default) rather than by renaming or removing. |
| `config.rs` | `Paths`, `Settings`, JSON helpers. Debug builds honour `TRUFFLE_DEV_DATA_DIR`. |
| `main.rs` | Runtime + eframe set-up, `--minimized` start, single instance, shutdown. |
| `icon.rs`, `build.rs`, `assets/`, `examples/make_icon.rs` | Window/tray icon (embedded PNGs), exe icon + version resource (rc.exe via winresource), icon generator. `build.rs` also sets `TRUFFLE_BUILD_DATE` (`src/build_date.rs`) for Settings → About. |
| `assets/licences.txt`, `examples/licences.rs` | The open-source licences Settings → About lists: every crate in the Windows build (from `cargo metadata`), Inter, yt-dlp and Node.js. Run `cargo run --example licences` after changing dependencies. |
| `session.rs` | librespot OAuth login + credential cache. |
| `engine.rs` | Player slots, mixer, `EngineEvent`s, crossfade hand-over. Submodules: `tracking` (event filtering), `output` (own rodio sink, device loss), `dsp` (equalizer + limiter, allocation-free), `gain` (fades applied at play time: crossfade, sleep-timer fade). |
| `library.rs` | Rootlist, playlist contents, batched track metadata, liked songs, disk cache by revision. |
| `webapi.rs` | Optional PKCE Web API client (user's own client id): rename/describe, cover upload, liked songs. |
| `queue.rs` | Fisher–Yates shuffle, rules, repeat, history, persistence types. Pure + unit tests. |
| `lastfm.rs` | Last.fm scrobbling: `Scrobbler` (connection, sending, backoff, status). Submodules: `api` (MD5 request signing, the four calls, error classes), `tracker` (pure play-time accounting), `queue` (persisted unsent scrobbles), `mock_tests` (end to end against a local mock server). |
| `listen.rs` | Listen along, pure: the channel description format, position maths, the host's write box (`Outbox`) and the listener's `Follow` rules. Submodule `links`: the friends' links (`LinkBook`) and the writes to them (`Fanout`). Unit-tested with a fake clock. |
| `controller.rs` | Orchestrates everything above; owns queue/history/engine; reconnects. Submodules: `playback`, `account`, `extras` (equalizer, crossfade timing, sleep timer), `scrobble` (feeds the `Scrobbler`, Last.fm commands), `listen` (listen along), `links` (the friends' listen-along links), `jobs`, `persist`. |
| `platform/` | Windows integration driven by the UI: `media` (SMTC overlay + media keys, souvlaki), `tray` (tray-icon + menu), `discord` (Rich Presence thread), `startup` (HKCU Run value, Start-menu .lnk via COM), `state` (pure change detection, timestamps, text fitting). |
| `ui/` | All egui code, image loading/caching for album art. `shell` (platform wiring, tray/hidden/mini window states), `window` (HWND lookup, geometry rules), `screens` (interface size per screen), `mini`, `eq`, `accent` (album-art accent), `settings`, `side`, `theme`, … |

## Find duplicates (`ui/dupes.rs`, `controller/browse.rs`, `library/editing.rs`, `library/edits.rs`)

- **Where**: the ··· menu of a playlist's page and the playlist right-click menu, for playlists
  the user can edit only. Not opened over another dialog (one Enter would answer both).
- **Reading** (`Command::FindDuplicates`; `Event::Duplicates`, `None` if it failed): the
  controller reads the playlist with `Library::playlist_now`, which always asks Spotify for its
  revision (the playlist list's may be older) and uses the saved copy only at that revision.
  Late answers from an older session are dropped (epoch). The UI groups it once, when it comes.
- **Matching**: same name (folded with `ui/fold.rs`, spaces trimmed and runs of them made one)
  and the same set of artists in any order (by URI when both have one, else by folded name).
  Local files are left out; a song Spotify no longer has (no artists) only matches itself. The
  copy added first stays (place decides ties and unknown dates); the others start ticked unless
  their length is more than 5 s off the one that stays (OTHER LENGTH).
- **Removing** (`Command::RemoveDuplicates` with the playlist as read and the ticked places):
  `Library::remove_rows` reads the playlist again under the write gate and takes out rows by
  place (`edits::rem_one`, highest first, 100 per change, on the revision just read), never by
  URI (that would take every copy). If the playlist changed, each row is found again as its
  song's same copy (`edits::rows_now`), which needs that song's copies all still there in the
  same order with the same dates; otherwise nothing more goes ("The playlist changed. Try
  again."). After a removal the saved copy is read again, so the page and the counts show it.

## Listen along (`listen.rs`, `controller/listen.rs`, `library/friends.rs`, `ui/friends.rs`, `ui/links.rs`)

- **Following** (`Command::ListenAlong { how, name }`, `ListenStop`; `Event::Listening`): `how` is a
  friend in Spotify's friend activity (`/presence-view/v1/user/{name}`, read every 5 s) or a
  Truffle host's channel playlist (its header read every 3 s). The user's queue waits aside; the
  host's song plays from a one-song queue (source: its album) at `position + (now - sent_at)`.
  Off by more than 2.5 s: seek (2 s right after a song starts, for YouTube's fetch). The end of
  the host's song is waited out. Play/pause/next/previous/seek/shuffle/queue commands are
  refused with a note while following. Stop (or the host stopping, 10 min of silence after their
  song, 5 failed reads, the headphones going away) brings the user's music back, paused where it
  was. A host clock running ahead (a state "sent" after now) is allowed for.
- **Sharing** (`ListenShare(bool)`, `Event::ListenShare { on, link }`): off by default. Every
  friend's link that isn't kicked gets `Listen along with me on Truffle. [tl1;<uri>;<ms>;<sent at
  ms>;<play|pause|stop>]` on every song change, pause, resume and seek (`flush_playback` compares
  with the state shared last). `listen::links::Fanout`: one `Outbox` per link (at most one write
  on its way and one a second, the newest wins) and a shared budget, 12 at once then 1 a second,
  because librespot allows about 300 calls per 30 s for all of Truffle (`http_client.rs`) and a
  write is 2 (header read, then the change). Stops go first and need no budget; a "slow down"
  pauses every link write for 30 s. Link writes (`Library::write_channel`) skip the write gate
  (nothing else edits link playlists) but take turns per link, so quitting's stop lands after a
  write already on its way. Turning sharing off writes "Not sharing right now." once to every
  live link. Quitting writes a stop to every link written to in that session, all at once, 1 s
  at most.
- **Links** (`ListenNewLink { name }`, `ListenRenameLink`, `ListenKick { id, kick }`,
  `ListenRemoveLink(id)`, `ListenRemakeLink(id)`; `Event::ListenLinks`; `controller/links.rs`,
  `ui/links.rs`): one channel playlist per friend, all named "Truffle · listen along"
  (made with `create_channel`, so not on the profile); the friend's name stays on this PC.
  Up to 10 per account. They live in `listen_links.json` (`listen::links::LinkBook`), not in
  settings.json, because an older Truffle saves all of settings.json at exit and drops keys it
  doesn't know. **Pause** (a link's switch turned off; `kick` in the code) = the ordinary stop written once (what every other link says when the
  music stops: "Not sharing right now." while sharing is off; owed until it lands, so it
  survives a restart and goes at the next login), then nothing; the friend sees "… stopped
  playing". **Turning it back on** writes what plays at once, to the same link (while a song loads, the
  kick's stop not sent yet is taken back, and the song is shared once it plays). The switch flips at
  once, so the second click of a double-click (or a held Enter) doesn't count. **Remove**: a stop, then the playlist leaves the library (Spotify's delete),
  owed (`Removed::delete_owed`, saved) until Spotify says it is out: one that fails, or is cut
  off by quitting or a lost connection, is asked for again at login, after reconnecting and
  after a library refresh, and a refresh that still finds a removed link's playlist takes it out
  too (Truffle hides them, so nothing else would). The uri is kept as a tombstone, never written
  or taken in again. A removed link never checked (owner unknown) that another account can't
  write keeps its stop for its own account. A link's playlist made as the session ended becomes
  such a tombstone, owing its stop, so it goes at that account's next login; one whose making
  failed after the playlist was made (`create_channel` says which) becomes one owing only its
  way out of the library. A link Spotify refuses for good shows "This link stopped working" (New
  link makes a new playlist for it; never by itself); one that can be read again later is said
  what it missed (a kicked one its stop), and if that happens while it is made anew, it keeps its
  playlist and the new one goes. A link made this session is never taken as gone; one made while
  a song loads says stopped until the song plays, then hears it. Links of another account are
  never written or shown. `Settings::listen_channel` is kept as the first link that isn't
  kicked, so older Truffles write to no kicked link; the one channel from before links became
  the "First link", and a channel an older Truffle makes is taken in as an "Older link" at the
  next start (neither has a date: when it was made isn't known). Every save also writes
  `listen_links.json.bak`. A damaged `listen_links.json` is set aside, and the copy used and
  saved as the file again at once (a start that changes no link saves nothing); a file gone
  with its copy there is the copy too. One that can't be opened (held by another program), or
  a gone or damaged one whose copy can't be, is never set aside or saved over: the copy is used,
  or it is read again at login; meanwhile no link can be made, kicked, let back in, renamed or
  removed (each says why: the change couldn't be saved, and the next start would undo it).
  Without either, the book starts empty (unknown links are never written). Logged out, the Friends
  tab shows "Log in to see your links", not "No links yet". The log names links by number only
  (never their playlist: it is the key to listening). Every list hides link playlists
  (`playlists_for_ui`), and Join refuses one's own links.
- **Friends** (`Friends`, `FriendPeek(user)`, `ListenJoin(link)`, `ListenForget(uri)`): the buddylist
  plus the hosts joined with a link (`Settings::listen_hosts`).
- **Friends tab** (`ui/friends.rs`, the links' rows and dialogs in `ui/links.rs`): the friends,
  Join with a link, and the sharing switch and links, in one of three layouts picked with the
  button at the right of the side panel's tabs (`Settings::friends_layout`; files from before it
  get Card): **Card** (a card with the switch, a summary line such as "2 links · Sam kicked" and
  MANAGE LINKS, which opens the links in the card, kept in `friends_links_open`), **List** (YOUR
  LINKS, then FRIENDS) and **Switch** ("Friends" | "Your listeners"). The New, Rename and Remove
  dialogs show over the main window. Settings > Accounts only says where it is, with OPEN (which
  closes Settings and opens the tab). The narrow window's Playing page has the same tab.
- **Test aid**: `truffle-selftest.request` with `listen state|share on|share off|join <link>|follow
  <user>|channel <link>|stop|friends`, and for links `listen links|newlink <name>|rename <n>
  <name>|kick <n>|unkick <n>|remove <n>|relink <n>` (`n`: the place in the Friends tab's list, from 1).
  The report (in `truffle-selftest.txt` only, never the log: it has the links) has one line per
  link (state, stop owed, last write, waiting, writing, link) and the budget. `channel <link>`
  follows one's own link too, for one-account tests.

## Platform (`platform/mod.rs`)

The UI owns one `Platform`, created in `TruffleApp::new` with the main window's HWND (found by
title on the UI thread; needed for SMTC).

- `update(&PlaybackState)` every logic pass: cheap, acts only on changes (track, play/pause,
  position jumps > 2 s). Sends song details to SMTC, the tray menu/tooltip, and the Discord thread.
- `poll()` returns `PlatformAction`s: `Command(..)` (forwarded to the controller), `ShowWindow`,
  `ToggleMiniPlayer`, `Quit`.
- `apply_settings(&Settings)` switches media controls, tray and Discord on/off and changes the
  Discord app id; a no-op unless one of those settings changed.
- Discord: only `type: Listening` with details/state/assets/timestamps is ever sent; never party,
  secrets, join/spectate, instance or buttons (unit-tested). Paused or stopped clears the status.
  Users supply their own Discord application id (16–21 digits). Discord's separate Spotify
  connection can still offer Listen Along; Settings explains how to turn that off in Discord.

## Last.fm (`lastfm.rs`, `controller/scrobble.rs`, `ui/settings/lastfm.rs`)

Last.fm API: <https://www.last.fm/api>. The user brings their own API key and shared
secret (32 hex characters each, checked in the UI and again in the controller).

- **Commands**: `ConnectLastfm { api_key, api_secret }`, `CancelLastfmConnect`,
  `DisconnectLastfm`, `SetLastfmEnabled`, `SendHeldLastfmScrobbles { to }`,
  `DiscardLastfmScrobbles { keep_for }`. The controller answers every one with an
  `Event::Lastfm(LastfmStatus)`, even when nothing changed: the UI shows the effect at once and
  takes that status as the confirmation. One status is also sent at start-up and on every
  change. Every status carries all fields.
- **Connect**: `auth.getToken`, open `last.fm/api/auth/?api_key=…&token=…` in the browser, poll
  `auth.getSession` every 3 s for up to 10 minutes (cancellable). On success the session goes to
  `lastfm_session.json`, and the controller saves the key, the secret and `lastfm_enabled = true`
  to the settings right away. An existing connection keeps working until then. The outcome
  (connected, or why not) also comes as a `Status` / `Error` toast, because clicking back into
  Truffle from the browser can close Settings.
- **Play counting**: `flush_playback` runs after every controller loop iteration (including the
  1 s tick) and hands the playback state plus `play_id` to the scrobbler. `play_id` changes on
  every new load and when "Previous" restarts a track, so repeats count as new plays. Listening
  time is wall-clock time while `Playing`; a gap of more than 5 s between observations (sleep)
  counts as nothing. Eligible: track > 30 s and heard ≥ min(half, 4 min). The sleep timer
  (when it paused music), logout and shutdown end the current play.
- **Sending**: an eligible play is appended to `lastfm_queue.json` (atomic write) before any
  send. Batches of up to 50, oldest first, one in flight, 1 s apart. Errors 11/16 and network
  failures back off 15 s → 30 min (±20 %), cut short when Spotify reconnects; 29 waits ≥ 5 min;
  ignored code 5 (daily limit) waits 1 h; other ignored codes drop the item; a malformed batch
  (6, 7, ...) is re-sent one item at a time. Items older than about 14 days are dropped.
  Shutdown queues but never starts a send (a cut-off send would be a duplicate at the next
  start). A batch already on its way is waited for up to 2 s after `Shutdown`
  (`Controller::finish_lastfm_send`, inside `main.rs`'s 3 s grace), so what Last.fm took is
  removed from the queue instead of being sent again.
- **Accounts**: each queued play records the Last.fm user it was played for (`user`; plays from
  before this have none and are given the saved sign-in's at load). Only the connected
  account's plays are sent. Plays of another account (after Disconnect and connecting as someone
  else) are `held`: Settings asks to send them to the connected account or discard them.
- **Errors that need the user**: 9 deletes `lastfm_session.json`; 4/10/13/26 stop sending
  ("stopped": `LastfmStatus::stopped` says why, `connected` is false, the user name stays).
  Settings then shows the reason with Connect again, Disconnect and the scrobbling switch; plays
  are still queued unless scrobbling is paused. The refusal code is saved in the session file
  (`refused`), so a later start shows "stopped" at once, doesn't toast again, and sends one
  batch to check whether Last.fm accepts Truffle again (if so, it resumes by itself). A
  successful connect or Disconnect clears it. Both errors keep the queue and send one
  `Event::Error` toast.
- **Queue file**: one that can't be parsed or read (after three tries) is renamed to
  `lastfm_queue.json.corrupt-<secs>`. If even that fails (another program holds it), it is
  never replaced: the next save first reads it again and merges its plays, or renames it, and
  writes nothing while neither works.
- **Secrets**: the shared secret and the session key never go into logs or URLs (custom
  `Debug` impls; `ConnectLastfm` is logged without its fields). The secret is only used for
  `api_sig`; requests are form-encoded `POST`s to `https://ws.audioscrobbler.com/2.0/`.

## Key constraints

- librespot `Session::new` / `Player::new` need a tokio runtime context.
- librespot exits the process (`exit(1)`) for non-Premium accounts after connect. The vendored
  `librespot-core` only logs it instead, so Free accounts keep the library and play from YouTube.
- librespot OAuth `get_access_token` blocks on a TcpListener: run it in `spawn_blocking`.
- `PlayerEvent` quirks: dedupe `EndOfTrack` per `play_request_id`, check `track_id` on `Unavailable`.
- Web API (Feb 2026 rules): Development Mode, ≤5 users, owner needs Premium; `/playlists/{id}/items`
  only for owned/collab playlists, max 50 per page; refresh tokens expire after 6 months
  (`invalid_grant` → re-login). Redirect `http://127.0.0.1:8899/callback`.
- Cover upload: base64 JPEG ≤ 256 KB, `Content-Type: image/jpeg`, scopes `ugc-image-upload` +
  `playlist-modify-public` + `playlist-modify-private`.
- egui 0.36: `App::ui(&mut self, ui, frame)` (no `update`), `egui::Panel::left/right/top/bottom`
  (no SidePanel/TopBottomPanel), `CornerRadius` (no Rounding), seek on `drag_stopped()`.
- Keep it light: no egui_extras http loader (fetch art with reqwest, cache to disk), forget images
  that scroll far out of view, virtualize lists (`ScrollArea::show_rows` / TableBuilder rows).

- eframe 0.36 ignores `with_visible(false)` for the first frame and doesn't re-export
  `raw-window-handle`: see the start-up notes above and `ui/window.rs`.
- `panic = "abort"` in release: nothing on the UI thread may panic (icon decoding, tray, SMTC
  and Discord failures are logged and the feature is left off).
- md-5 0.11 / digest 0.11: the hash output has no `LowerHex`, so `format!("{:x}")` doesn't
  compile; `lastfm::api::hex` encodes the bytes. Last.fm signs parameters in byte order
  (`BTreeMap<String, String>`: `artist[10]` before `artist[1]`), without `format`/`callback`.

## Data locations

`%APPDATA%\truffle\config\` settings.json, state.json, webapi_token.json, librespot/ credentials,
lastfm_session.json (user name + session key, plus `refused` while Last.fm refuses the API
account), lastfm_queue.json (unsent scrobbles, `{"version":1,"items":[…]}`, oldest first, each
with the Last.fm `user` it was played for, at most 10,000), listen_links.json and its copy
listen_links.json.bak (the friends' listen-along links: their names, only on this PC, each link's
playlist, kicks, stops owed, removed links; private: each link is the key to listening along),
like_queue.json (likes and unlikes not sent yet, `{"version":1,"accounts":{"<user>":[…]}}`,
oldest first, the latest change per song; see `library/likes.rs`).
`%LOCALAPPDATA%\truffle\cache\` tracks/ (per-source JSON), audio/ (librespot), images/.
Only when switched on in Settings: `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` value
`Truffle` (`"<exe>" --minimized`), and `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Truffle.lnk`.
Debug builds use `Truffle (dev)` for both, so they never touch the release build's entry.
Debug builds with `TRUFFLE_DEV_DATA_DIR` set use `<dir>\config` and `<dir>\cache` instead.
