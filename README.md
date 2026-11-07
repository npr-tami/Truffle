# Truffle

Truffle is a small, fast Spotify player for Windows, written in Rust. It is a single native
window (egui with OpenGL). It does not embed a browser.

It is built around **true shuffle**. Shuffle is a uniform Fisher–Yates shuffle, so every order of
your playlist is equally likely. On top of that are a few rules you can switch on or off:

- **Play every song before any repeats.** Truffle remembers what you've heard, even after a
  restart, until the whole list has played.
- **Avoid the same artist twice in a row** when possible.
- **Favour songs I rarely play**, using Truffle's own play counts.
- **Skip songs I've blocked.** Right-click a song and choose *Block from shuffle*.

You can also browse your playlists and Liked Songs, like and unlike songs, filter long lists
instantly, and resume where you left off. If you set up the optional editing connection
(described below), you can also rename playlists, change their descriptions and upload covers.

**Find duplicates** (in one of your own playlists, in its ··· menu or when you right-click it)
lists the songs that are in it more than once: the same song added twice, or a single and the
album version of it. Same song means the same name and the same artists; capitals and accents
don't matter, but "Remastered" or "Live" makes it another song. The copy you added first stays
and the others are ticked, except a copy of a different length (maybe a remix or an edit). You
can change the ticks, then **Remove** takes out exactly the ticked copies.

It also fits into Windows and your day:

- **Media keys and the Windows media overlay** (the one that appears with the volume keys, and
  on the lock screen).
- **A tray icon** with play/pause, next, previous, the mini player and Quit. Closing the window
  can keep the music playing in the tray.
- **A mini player**: a small always-on-top window with the cover, the controls and the seek bar.
- **Start with Windows** (hidden in the tray) and a **Start-menu shortcut**.
- **An equalizer** (10 bands, presets), **crossfade** between songs, and a **sleep timer**.
- **Listen along** in the Friends tab of the side panel: hear what a friend plays, or give each
  friend their own link to hear you. Pause one with their switch and turn it back on any time.
- **A Discord status** that shows what you're listening to. Nobody can join or control it.
- **Last.fm scrobbling** with your own free Last.fm API account. Plays are saved and sent
  later if Last.fm can't be reached.
- **Looks**: dark or light theme, an accent colour (or one taken from the album art), compact
  lists and your own colours.

> Truffle plays music through [librespot](https://github.com/librespot-org/librespot), an
> **unofficial**, open-source Spotify client library. See [About librespot](#about-librespot).

## Requirements

- **Windows 10 or 11** (64-bit) with OpenGL 2+ graphics (any normal GPU driver).
- **A Spotify account.** With Premium, songs play from Spotify. A Free account can browse its
  library, likes and search as usual, and its songs play from YouTube instead (see
  [Sound from Spotify or YouTube](#sound-from-spotify-or-youtube)).
- To build it yourself:
  - **Rust 1.95 or newer**, stable, MSVC toolchain. Install it from <https://rustup.rs>.
  - **Visual Studio Build Tools** with the "Desktop development with C++" workload, for the MSVC
    linker, a C compiler and the Windows SDK (its `rc.exe` puts the icon into `truffle.exe`).

## Build and run

```powershell
git clone <this repository> truffle
cd truffle
cargo build --release
.\target\release\truffle.exe
```

The release build is one self-contained `truffle.exe` of about 15 MB, with no installer and no
runtime files. You can copy it anywhere. It carries its own icon and version details. If the
Windows SDK's `rc.exe` can't be found, the build still succeeds and prints a warning; only the
exe's icon in Explorer is missing then (the window and tray icons are built in).

Only one Truffle runs at a time. Starting it again brings the running one to the front.

For development, run `cargo run`. The debug build keeps a console window open for log output.
To see more detail in the log:

```powershell
$env:RUST_LOG = "truffle=debug"; cargo run
```

A debug build can run next to an installed release build. To keep it from sharing your login,
settings and play history, point it at a folder of its own (debug builds only):

```powershell
$env:TRUFFLE_DEV_DATA_DIR = "$env:TEMP\truffle-dev"; cargo run
```

A debug build's startup entry and Start-menu shortcut are named `Truffle (dev)`, so trying those
settings never changes the ones of your release build.

Run the tests with `cargo test`. The test that plays real audio is ignored unless you ask for it.
The icon artwork is drawn by `cargo run --example make_icon`, which rewrites the files in
`assets\`.

## First login

1. Start Truffle and click **Log in with Spotify**.
2. Your browser opens Spotify's own login page. Log in and approve.
3. Spotify sends the browser back to `http://127.0.0.1:8898/login` on your own computer. The tab
   says *Logged in — you can close this tab*. Go back to Truffle.

Truffle saves the reusable login that Spotify returns, so later starts connect silently. You have
5 minutes to finish in the browser. While you log in, port **8898** must be free. If another
librespot-based tool is running, close it first.

To log out, click your name at the top left and choose **Log out**, or use Settings. This
deletes the saved login from your computer.

## Optional: playlist editing (Spotify Web API)

librespot handles playback and reading your library. Renaming playlists, changing descriptions
and uploading covers use Spotify's **official Web API**, which needs a free developer
*Client ID* of your own. Setup takes about 3 minutes and only has to be done once. Truffle
also shows these steps in **Settings → Editing**.

1. Open <https://developer.spotify.com/dashboard> and log in with your normal Spotify account.
   Accept the Developer Terms if asked.
2. Click **Create app**:
   - Use any name and description, for example "Truffle (personal)".
   - For **Redirect URIs**, enter exactly **`http://127.0.0.1:8899/callback`** and click
     **Add**. It must be `127.0.0.1` (not `localhost`), with `http` and port `8899`.
   - Tick **Web API**, accept the terms and click **Save**.
3. Open the app's **Settings** and copy the **Client ID** (32 letters and digits). Truffle
   never needs the Client *secret*, so don't paste it anywhere.
4. Under **User Management**, add the email address of your Spotify account. The app owner is
   usually allowed already, but adding yourself avoids a "user not registered" error.
5. In Truffle, click your name, then **Settings**, then **Editing**. Paste the Client ID and
   click **Connect**. Your browser opens Spotify's consent page. Click **Agree**, then return
   to Truffle.

Things to know about Spotify's rules for personal apps (Development Mode):

- **At most 5 users per Client ID.** Each person must be added under User Management, and the
  app owner must keep a Premium subscription. If it lapses, every request fails with 403. The
  simplest setup is for everyone to make their own app.
- **The connection expires about 6 months after you approve it.** Spotify doesn't extend it.
  Truffle reminds you two weeks before. To renew, open Settings, click **Disconnect**, then
  **Connect** again.
- You can only edit playlists that you own or collaborate on.
- While you connect, port **8899** must be free.

Everything else (playback, shuffle, browsing, liking songs) works without this step. If
Spotify's internal route for Liked Songs is unavailable, Truffle can fall back to the Web API
for that too, but only once this connection is set up.

## Windows features

All of these are in **Settings → Windows** (click your name, then **Settings**).

- **Media controls** (on by default): the song, artist and cover appear in the Windows media
  overlay and on the lock screen. Your keyboard's play/pause, next and previous keys control
  Truffle, also while it's hidden.
- **Tray icon** (on by default): right-click it for the current song, Play/Pause, Next,
  Previous, Show Truffle, Mini player and Quit. Click it to bring the window back. Hovering it
  shows the song.
- **Closing the window keeps Truffle playing in the tray** (off by default; needs the tray
  icon). The window's × then hides Truffle instead of quitting. To quit, right-click the tray
  icon and choose **Quit**. If the tray icon can't be shown, closing always quits, so you are
  never left without a way back.
- **Start Truffle when I sign in to Windows**: Truffle starts hidden in the tray (or minimised,
  if the tray icon is off). It appears in Task Manager's *Startup apps* as "Truffle"; turning it
  on here also re-enables it there.
- **Add Truffle to the Start menu**: creates a Truffle shortcut in the Start menu, so you can
  find it by typing its name.

### Mini player

Press `Ctrl` + `M`, click 🗗 in the player bar, or choose **Mini player** in the tray menu. The
mini player is a small window with the cover, title, artist, the controls, the heart and the
seek bar. Drag it anywhere; Truffle remembers where. Double-click it, click 🗖 or press
`Ctrl` + `M` to go back to the full window, which comes back at its old size and place. The 📌
button keeps it on top of other windows (on by default). A ⚠ appears when there is an error
message to read; click it to open the full window.

## Sound from Spotify or YouTube

Your library, likes and search always come from Spotify. The sound itself can come from Spotify
(Premium) or from YouTube: Truffle finds the same song on YouTube and plays its audio. A Free
account always uses YouTube; with Premium you can pick it in **Settings → Sound → Sound from**.

YouTube sound needs two free programs:

- **[yt-dlp](https://github.com/yt-dlp/yt-dlp/releases)**: put `yt-dlp.exe` next to
  `truffle.exe` (Truffle also finds it on `PATH` or in your Downloads folder).
- **A JavaScript runtime** for yt-dlp: [Node.js](https://nodejs.org) or
  [Deno](https://deno.com). Installing either is enough.

**Settings → About** shows whether both were found. Truffle picks the best match by length,
title and artist, and avoids live, cover and sped-up versions unless the Spotify title has them.
If it picks the wrong video, right-click the song and choose **Wrong song? Pick another**;
Truffle remembers your pick. Songs are kept in the audio cache, and the next few in the queue are
fetched ahead, so they start quickly.

## Sound: equalizer, crossfade and sleep timer

**Equalizer** (Settings → Sound): ten bands from 31 Hz to 16 kHz, ±12 dB, plus a preamp
("Pre"). Pick a preset (Bass boost, Vocal, Rock, Late night and others) or drag the bands
yourself. Changes are heard after about half a second. If boosted songs crackle, lower the
preamp. Truffle also limits the output so boosts can't clip harshly.

**Crossfade** (Settings → Sound): 0 to 12 seconds. Each song fades into the next when it ends
by itself; skipping a song still changes at once. **Off** (0) plays songs back to back without
a gap.

**Sleep timer**: click 🌙 in the player bar and choose 15, 30, 45, 60 or 90 minutes, or **End
of this song**. The time left shows next to the moon. A timed stop fades the music out over its
last 30 seconds, then pauses; the volume is back to normal the next time you press play. Choose
**Turn off** in the same menu to cancel.

## Discord status

Truffle can show the song you're playing in your Discord status ("Listening to …"), with the
artist, the album cover (hover it for the album name) and a progress bar. Pausing clears it.
**It never offers "Join" or "Listen Along"**: nobody can join your session or control your music.

Discord needs an "application" of your own to show the status under. It is free and takes a
minute. Truffle also shows these steps in **Settings → Discord**.

1. Open <https://discord.com/developers/applications> and log in with your Discord account.
2. Click **New Application** and give it a name, for example "Truffle". Friends may see this
   name next to the song.
3. On its **General Information** page, copy the **Application ID** (a long number).
4. In Truffle, open **Settings → Discord**, tick **Show what I'm listening to on Discord** and
   paste the Application ID. The status line underneath says **Connected** once it works.

The Discord desktop app must be running on the same PC. If it isn't, Truffle tries again every
15 seconds, so you can start Discord before or after Truffle. In Discord, **Settings → Activity
Privacy → Share my activity** must be on.

> **About Listen Along:** if you have also linked Spotify in Discord (**Settings →
> Connections**), Discord can show its *own* Spotify status for the same music, and that one
> does offer Listen Along to friends. Truffle can't turn that off. If you don't want anyone to
> join, open Discord's **Settings → Connections → Spotify** and turn off **Display Spotify as
> your status**, or remove the Spotify connection.

## Last.fm scrobbling

Truffle can scrobble the songs you play to your Last.fm profile and show them there as playing
now. Like the Discord status, it uses a free API account of your own, so Truffle never sees
your Last.fm password. Setup takes about 2 minutes, once. Truffle also shows these steps in
**Settings → Last.fm**.

1. Open <https://www.last.fm/api/account/create> and log in to Last.fm if asked.
2. For **Application name**, enter "Truffle". Last.fm shows this name when it asks you to
   approve the app. Leave **Callback URL** and **Application homepage** empty, then submit.
3. The page shows an **API key** and a **Shared secret**. Each is 32 letters and digits, and
   they are different codes. Your API accounts are listed later at
   <https://www.last.fm/api/accounts>.
4. In Truffle, open **Settings → Last.fm**, paste both and click **Connect to Last.fm**. Your
   browser opens Last.fm's approval page. Allow Truffle access, then return to Truffle.
   Settings shows **Connected as** *your name* within a few seconds. Truffle waits up to
   10 minutes for the approval; **Cancel** stops waiting. A message also tells you when it
   worked (or why not), in case Settings closed while you were in the browser.

Truffle keeps the API key and shared secret only after a connection works, so a typo is never
saved. **Disconnect** deletes Truffle's Last.fm sign-in. The key, the secret and any plays not
sent yet are kept. To also remove Truffle's access on Last.fm's side, open Last.fm's
**Settings → Applications**.

### What counts as a play

- A song counts once it is **longer than 30 seconds** and you have heard **half of it, or
  4 minutes**, whichever comes first. Paused time and parts you skip over don't count. Parts
  you go back and hear again do.
- The scrobble's time is when the song **started**.
- Every play counts on its own: repeat-one, playing a song again, and **Previous** restarting
  it (after 3 seconds).
- Only the **first artist** is sent, the same as Spotify's own Last.fm connection does. The
  title and album are sent exactly as Spotify shows them, and Last.fm corrects names itself.
- Podcast episodes are never scrobbled. Local files are scrobbled only if they have an artist
  and a title.
- Time while the computer sleeps doesn't count. When the sleep timer pauses the music, that
  play ends there; resuming later is a new play.
- **Now playing** is sent when a song starts, and again when you resume after a long pause.
- Untick **Scrobble my plays** to pause scrobbling and now playing without disconnecting.
  Songs played while it is off, or while Truffle isn't connected, are not sent later. (While
  scrobbling is *stopped* because Last.fm refuses Truffle, see below, they are kept.)

Spotify's own Last.fm connection (if you set it up on Last.fm) can't see what Truffle plays,
because librespot doesn't report plays to Spotify. So nothing is counted twice, and your phone
and other Spotify apps keep scrobbling through that connection as before.

### Offline and unsent plays

- Each play is saved to `%APPDATA%\truffle\config\lastfm_queue.json` the moment it counts,
  before it is sent. A crash, a lost connection or quitting never loses it.
- Unsent plays are sent oldest first, up to 50 at a time. When Last.fm can't be reached,
  Truffle tries again after 15 seconds, then less often, up to every 30 minutes. It tries
  again right away when the Spotify connection comes back, and at the next start.
  **Settings → Last.fm** shows how many plays are waiting.
- Quitting doesn't start a send. A send cut off halfway could be sent twice, so the plays wait
  for the next start instead. A send that is already on its way when you quit gets up to
  2 seconds to finish, so what Last.fm took isn't sent again.
- Last.fm doesn't take scrobbles older than about two weeks, so older ones are dropped, and
  Settings says how many. Songs that Last.fm filters out are dropped too. If Last.fm's daily
  scrobble limit is reached, the rest wait an hour.
- Plays are only sent to the Last.fm account they were played on. If you disconnect and then
  connect a different account, Settings shows the earlier account's waiting plays and asks
  whether to **send them** to the new one or **discard** them. While no account is connected,
  waiting plays can be discarded there too.
- If the file of unsent plays can't be read at start (for example because another program has
  it open), Truffle never overwrites it: it reads it again before saving new plays.

### When Last.fm stops accepting Truffle

- If you removed Truffle's access on Last.fm, Truffle forgets the sign-in and a message asks
  you to connect again. The plays not sent yet are kept and sent once you do.
- If Last.fm refuses the API key or shared secret (you reset the secret, deleted the API
  account, or Last.fm suspended the key), scrobbling **stops** and a message says why, once.
  **Settings → Last.fm** then shows the reason with three ways on:
  - paste a corrected key and secret (or those of a new API account) and click
    **Connect again**;
  - untick **Scrobble my plays** to stop keeping the songs you play meanwhile (while it is
    ticked, they are kept and sent once Last.fm accepts Truffle again);
  - **Disconnect**, if you don't want to scrobble any more.
- Truffle remembers the stop, so it isn't announced again at every start. Each start checks
  once whether Last.fm accepts Truffle again, and resumes by itself if it does.

### Privacy

- Truffle sends Last.fm only what it needs to scrobble: the artist, title, album and length of
  each song you play, when you started it, your API key and the session key from the approval.
  Nothing about your Spotify account is sent.
- The **shared secret is never sent**. It only signs the requests. Truffle never asks for your
  Last.fm password: you log in on Last.fm's own page.
- The API key and shared secret are stored in `settings.json`, and the session key in
  `lastfm_session.json`. Both are plain text, like the Spotify login files, so keep them
  private. The secret and the session key are never written to the log. **Show** next to the
  shared secret only lasts while Settings is open.
- Scrobbles appear on your Last.fm profile. Last.fm's own privacy settings decide who can see
  them.

## Looks

In the panel on the right, under **Appearance**:

- **Theme** (dark or light) and an **accent colour**.
- **Match the accent to the album art**: the accent follows the cover of the playing song,
  fading over smoothly, and is adjusted so it stays readable.
- **Compact lists**: smaller rows, so more songs and playlists fit.
- **Interface size**: each screen keeps its own. On a big screen Truffle makes itself bigger,
  by the screen's height in Windows' own units (1440 high: 125%, a 4K screen at 100% scaling:
  175%; never smaller than 100%), and says so once. `Ctrl` + `+` / `-` change the size on the
  screen the window is on, `Ctrl` + `0` goes back to the picked one. **Grow with the window**
  (off at first) makes it bigger still on a wide window, up to 1.5 times, once you stop
  resizing.
- **Custom colours**: change the background, panels, text, secondary text and hover colours
  one by one. Each has its own reset, and **Reset all** goes back to the theme. Text that would
  become hard to read is adjusted automatically.

Above the playlist list, **All / Mine / Others** shows every playlist, only the ones you made,
or only the ones made by other people. Truffle remembers your choice.

## Keyboard shortcuts

| Keys | Action |
|---|---|
| `Space` | Play / pause |
| `Ctrl` + `→` | Next song |
| `Ctrl` + `←` | Previous song (restarts the song if more than 3 s have played) |
| `Ctrl` + `M` | Switch to the mini player and back |
| Media keys | Play / pause, next, previous (also while Truffle is hidden; needs **Media controls**) |
| `Ctrl` + `F` | Filter the song list |
| `Ctrl` + `L` | Search playlists |
| `Esc` | Clear and leave the filter or search box |
| `Ctrl` + `+` / `Ctrl` + `-` / `Ctrl` + `0` | Make the interface larger / smaller / back to the size picked for this screen (each screen keeps its own) |
| Double-click a song | Play it (keeps the current shuffle order if that list is already playing) |
| Right-click a song | Play, like, block from shuffle, or copy its Spotify link |
| Click the playing song's title | Show it in its list |
| Double-click the mini player | Back to the full window |

Shortcuts are ignored while you type in a text box or while a dialog is open. In the mini
player only `Space`, `Ctrl` + `→`, `Ctrl` + `←` and `Ctrl` + `M` apply.

## Where your data is stored

Nothing is stored next to the exe. Truffle uses two folders:

**`%APPDATA%\truffle\config\`** (settings and logins):

| File | Contents |
|---|---|
| `settings.json` | Volume, shuffle/repeat, shuffle rules, theme, accent colour, interface size, audio options, last opened list, editing Client ID, and the options below |
| `state.json` | Play counts, blocked songs, "played this round" lists, and the saved queue and position |
| `librespot\credentials.json` | The reusable Spotify login. Logging out deletes it |
| `librespot\device_id` | A stable device id, so Spotify sees Truffle as one device |
| `webapi_token.json` | The editing connection's token, if you set it up. **Disconnect** deletes it |
| `lastfm_session.json` | Your Last.fm user name and session key, if you connected Last.fm (and, while scrobbling is stopped, Last.fm's error code). **Disconnect** deletes it |
| `lastfm_queue.json` | Plays Last.fm hasn't accepted yet, oldest first, each with the Last.fm account it was played on (at most 10,000; dropped after about two weeks) |
| `listen_links.json`, `listen_links.json.bak` | Your listen-along links: the name you gave each friend (only on this PC), each link, who is kicked, and removed links. The `.bak` is a copy written with every save |
| `like_queue.json` | Likes and unlikes Spotify hasn't taken yet, oldest first, per Spotify account. Truffle keeps sending them |

**`%LOCALAPPDATA%\truffle\cache\`** (safe to delete at any time):

| Path | Contents |
|---|---|
| `playlists.json`, `tracks\` | Your playlist list and each list's tracks, keyed by revision, so lists open instantly |
| `liked_snapshot.json` | Your liked-song ids, so hearts show at once on start |
| `images\` | Album and playlist art, trimmed to 200 MB when it grows past 300 MB |
| `audio\` | librespot's audio cache. Its size is set in Settings (default 1 GB, 0 turns it off) |
| `youtube\` | Songs' audio for YouTube sound, one file per video, within the same size (the least recently played go first) |

The newer options in `settings.json` (all are set from the app; a missing field takes its
default):

| Field | Default | Meaning |
|---|---|---|
| `playlist_filter` | `"All"` | Sidebar filter: `"All"`, `"Mine"` or `"Others"` |
| `equalizer` | off, flat | `enabled`, `gains_db` (10 bands, -12 to 12), `preamp_db` (-12 to 0), `preset` |
| `crossfade_secs` | `0` | Crossfade length, 0 (off) to 12 |
| `media_controls` | `true` | Media overlay, lock screen and media keys |
| `tray_icon` | `true` | Tray icon |
| `close_to_tray` | `false` | Closing the window keeps Truffle playing in the tray |
| `start_with_windows` | `false` | Mirrors the Windows startup entry (Truffle checks the real entry at start) |
| `discord_status` | `false` | Discord status on or off |
| `discord_app_id` | `null` | Your Discord Application ID |
| `lastfm_enabled` | `true` | **Scrobble my plays** (has an effect only once Last.fm is connected) |
| `lastfm_api_key`, `lastfm_api_secret` | `null` | Your Last.fm API key and shared secret, saved after a connection works |
| `dynamic_accent` | `false` | Accent colour from the album art |
| `compact` | `false` | Compact lists |
| `screen_sizes` | `{}` | Interface size set on each screen, by its name and resolution (e.g. `\\.\DISPLAY1 3840x2160`); a screen not in it gets a size from its height |
| `grow_with_window` | `false` | **Grow with the window** |
| `big_screen_tips` | `[]` | Screens Truffle already said it made itself bigger on |
| `custom_colors` | `null` | `background`, `panel`, `text`, `text_weak`, `hover` as `[r, g, b]` |
| `mini_player_on_top` | `true` | Mini player stays on top |
| `mini_player_pos`, `main_window_size` | `null` | Where the mini player was, and the main window's size |

Outside these folders, Truffle only writes what you switch on in Settings → Windows: the
startup entry `Truffle` under `HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run`,
and the shortcut `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Truffle.lnk`.

To remove Truffle completely, first turn off **Start Truffle when I sign in to Windows**, then
delete `truffle.exe`, both folders and the Start-menu shortcut (if you added it). If you
connected Last.fm, also remove Truffle under Last.fm's **Settings → Applications**.

The login and token files let anyone with access to your Windows account use your Spotify
session (and `lastfm_session.json` with `settings.json`, your Last.fm scrobbling), so keep
them private, like a browser profile. Keep `listen_links.json` private too: each link in it lets
anyone listen along with you, and it has your friends' names.

## Troubleshooting

- **Reporting a problem:** **Settings → About → Copy details** copies Truffle's version and build
  date, the Windows version, Premium or Free, where the sound comes from and the yt-dlp and
  JavaScript versions (no names, folders or keys), to send with a screenshot. **Open log** there
  opens `%LOCALAPPDATA%\truffle\logs\truffle.log`.
- **"YouTube mode needs yt-dlp":** put `yt-dlp.exe` next to `truffle.exe`, and install Node.js
  or Deno. **Settings → About** shows what Truffle found.
- **"another program is using port 8898/8899":** close other Spotify tools, or another Truffle
  window, and try again.
- **"INVALID_CLIENT: Invalid redirect URI" while connecting editing:** the Redirect URI on the
  dashboard isn't exactly `http://127.0.0.1:8899/callback`.
- **403 / "user may not be registered":** add your Spotify account's email under User
  Management for your developer app.
- **Playback pauses with a message about speakers or headphones:** the audio device went away.
  Plug it back in, or pick another default output in Windows, and press play. Truffle uses the
  Windows default output device each time playback starts.
- Quality, normalisation and audio cache changes apply after a restart (YouTube songs' cache
  follows a new size at once).
- **Truffle seems to be gone after closing it:** with "keep playing in the tray" on, it is still
  running. Click its tray icon (it may be under the ^ arrow on the taskbar), or start Truffle
  again, which brings the running one back.
- **The Discord status doesn't show:** check that the Discord desktop app is running on this PC,
  that Settings → Discord in Truffle says **Connected**, that music is playing (a paused song
  shows nothing), and that Discord's **Activity Privacy → Share my activity** is on. "Discord
  didn't accept this application ID" means the number isn't an Application ID; copy it again
  from the Developer Portal.
- **Friends see "Listen Along" on my status:** that is Discord's own Spotify connection, not
  Truffle. See [About Listen Along](#discord-status).
- **Media keys control another app:** Windows sends them to the app that played most recently.
  Press play in Truffle once, or check that **Media controls** is on in Settings → Windows.
- **"Last.fm didn't accept the API key" (or "the shared secret"):** copy both again from
  <https://www.last.fm/api/accounts>. The API key and the shared secret are different codes.
- **"The Last.fm approval expired":** click **Connect to Last.fm** again and allow access in
  the browser within 10 minutes.
- **Plays don't appear on Last.fm:** check that Settings → Last.fm says **Connected as** and
  that **Scrobble my plays** is ticked. A song counts only after half of it (or 4 minutes) has
  played. If plays are shown as waiting, Last.fm couldn't be reached; they are sent later.
  Plays recorded while another Last.fm account was connected wait until you choose to send
  them (Settings → Last.fm).
  Last.fm also drops scrobbles whose time looks wrong, so check that the computer's clock and
  time zone are right.

## About librespot

Truffle uses librespot for login, playback and reading your library. librespot is a
community-made, open-source implementation of Spotify's private client protocol. **Spotify does
not make, support or endorse it, or Truffle.** In practice:

- Spotify can change its private protocol at any time, which may break playback until
  librespot (and then Truffle) is updated.
- Using unofficial clients may go against Spotify's Terms of Use. Use Truffle at your own
  discretion, with your own Premium account.
- Truffle does not download music for offline use and does not remove or skip anything a
  Premium account would get. It streams what your subscription already allows.

The optional editing features use Spotify's official, documented Web API, with a developer
app that you create and control.

## Project layout

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md). In short: `ui/` (egui) talks to `controller`
through commands and events. The controller drives `session`/`engine` (librespot login and
playback, equalizer, crossfade), `library` (playlists, tracks, caches), `queue` (shuffle and play
order), `webapi` (optional editing) and `lastfm` (scrobbling). `platform/` is the Windows side
(media overlay, tray, Discord, startup entry), driven by the UI.

## License

MIT. See [LICENSE](LICENSE). What changed in each version is in [CHANGELOG.md](CHANGELOG.md).
