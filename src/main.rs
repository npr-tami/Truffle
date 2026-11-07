#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// `build.rs` uses it to make TRUFFLE_BUILD_DATE; compiled here only for its tests.
#[cfg(test)]
mod build_date;
mod canvas;
mod config;
mod controller;
mod engine;
mod icon;
mod lastfm;
mod library;
mod listen;
mod lrclib;
mod model;
mod platform;
mod queue;
mod session;
mod text;
mod ui;
mod video;
mod webapi;
mod youtube;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;

use crate::config::{Paths, Settings};
use crate::controller::UiLink;
use crate::model::Command;

/// How long shutdown waits for the controller, and then for leftover background work.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// Passed by the start-with-Windows entry: start hidden in the tray (or minimised when the tray
/// icon is off).
const MINIMIZED_ARG: &str = "--minimized";

/// `--play "<playlist name>"` (or `--play=<name>`): start that playlist once the library loads.
const PLAY_ARG: &str = "--play";

fn play_arg(mut args: impl Iterator<Item = String>) -> Option<String> {
    while let Some(arg) = args.next() {
        let value = if arg == PLAY_ARG {
            args.next()
        } else {
            arg.strip_prefix("--play=").map(str::to_owned)
        };
        if let Some(name) = value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty()) {
            return Some(name);
        }
    }
    None
}

/// Release builds log to `%LOCALAPPDATA%\truffle\logs\truffle.log` (the previous run's log is kept
/// as `truffle.previous.log`), since a windowed app has no console. Debug builds log to stderr.
/// Called only by the instance that will run, so a second launch never touches the live log.
fn init_logging() {
    let mut builder = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn,truffle=info"));
    if !cfg!(debug_assertions)
        && let Some(file) = open_log_file()
    {
        builder.target(env_logger::Target::Pipe(Box::new(file)));
    }
    builder.init();
}

fn open_log_file() -> Option<std::fs::File> {
    let log = config::log_file()?;
    let dir = log.parent()?;
    std::fs::create_dir_all(dir).ok()?;
    let _ = std::fs::rename(&log, dir.join("truffle.previous.log"));
    std::fs::File::create(log).ok()
}

fn main() -> Result<()> {
    let minimized_arg = std::env::args_os().skip(1).any(|a| a == MINIMIZED_ARG);
    if let Some(name) = play_arg(std::env::args().skip(1)) {
        ui::set_autoplay(name);
    }

    // One Truffle per Windows session. A second launch brings the running one to the front
    // (unless it is the start-with-Windows launch, which just leaves quietly).
    let Some(mut instance) = single_instance::Instance::acquire(!minimized_arg) else {
        return Ok(());
    };
    init_logging();
    log::info!("Truffle {} starting", env!("CARGO_PKG_VERSION"));

    let paths = Paths::new()?;
    log::info!("data in {} (settings, logins) and {} (caches)", paths.config.display(), paths.cache.display());
    paths.remove_stale_temp_files();
    let mut settings = Settings::load(&paths);
    if lastfm::restore_credentials(&paths, &mut settings)
        && let Err(e) = settings.save(&paths)
    {
        log::warn!("could not save the restored Last.fm settings: {e:#}");
    }
    let start_mode = StartMode::new(minimized_arg, settings.tray_icon);
    // Logical pixels (egui points at zoom 1.0, which is what the window is created with).
    let window_size = ui::initial_window_size(&settings);
    let icon_style = icon::style_for(settings.app_icon, platform::windows_taskbar_dark().unwrap_or(true));
    let settings = Arc::new(Mutex::new(settings));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("truffle-worker")
        .build()?;

    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Command>();
    let (event_tx, event_rx) = std::sync::mpsc::channel();

    // No `with_visible(false)` for the hidden start: eframe always creates the window hidden and
    // shows it after the first painted frame regardless. `start_mode.apply` handles it instead.
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(ui::WINDOW_TITLE)
            .with_app_id("truffle")
            .with_icon(Arc::new(icon::window_icon(icon_style)))
            // Truffle draws its own title bar (the Final design's chrome sign). Made without Windows'
            // from the start: taking it off later leaves the frame's old place until the window moves.
            .with_decorations(false)
            .with_inner_size(window_size)
            .with_min_inner_size(ui::MIN_WINDOW_SIZE),
        ..Default::default()
    };

    let handle = runtime.handle().clone();
    let shutdown_tx = cmd_tx.clone();
    // The controller task, kept so shutdown can wait for it to save and release the audio device.
    let mut controller_task: Option<tokio::task::JoinHandle<()>> = None;
    let task_slot = &mut controller_task;
    let instance_slot = &mut instance;
    let result = eframe::run_native(
        "Truffle",
        options,
        Box::new(move |cc| {
            // The window exists (still hidden) but no frame has been painted yet.
            match win32::find_own_window(ui::WINDOW_TITLE) {
                Some(hwnd) => {
                    // Back where it was when Truffle closed (if that screen is still there).
                    let place = settings.lock().unwrap_or_else(std::sync::PoisonError::into_inner).main_window_rect;
                    if let Some(rect) = place {
                        ui::restore_window_place(hwnd, rect);
                    }
                    // PowerShell's CloseMainWindow and the like can pick winit's hidden helper
                    // window instead of this one.
                    ui::forward_helper_close(hwnd);
                    start_mode.apply(hwnd)
                }
                None => log::warn!("could not find the main window; starting normally"),
            }
            instance_slot.listen(cc.egui_ctx.clone());
            let link = UiLink { events: event_tx, ctx: cc.egui_ctx.clone() };
            *task_slot = Some(handle.spawn(controller::run(paths.clone(), settings.clone(), cmd_rx, link)));
            Ok(Box::new(ui::TruffleApp::new(cc, cmd_tx, event_rx, settings, paths, handle.clone())))
        }),
    );

    // The window is gone: from here on a new launch waits for this process to finish saving and
    // exit, then starts normally (instead of asking a closed window to show itself).
    instance.stop_listening();

    // Let the controller handle Shutdown (save state, stop audio, close the session) before the
    // runtime goes away. Its Drop impl still saves if this times out.
    let _ = shutdown_tx.send(Command::Shutdown);
    // The timeout is created inside `block_on`: tokio timers need the runtime's context.
    if let Some(task) = controller_task
        && runtime.block_on(async { tokio::time::timeout(SHUTDOWN_GRACE, task).await }).is_err()
    {
        log::warn!("the controller did not stop within {SHUTDOWN_GRACE:?}");
    }
    runtime.shutdown_timeout(SHUTDOWN_GRACE);
    drop(instance);

    result.map_err(|e| anyhow::anyhow!("UI error: {e}"))
}

/// Set when a second launch asked this instance to come to the front.
static ACTIVATION_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Set while a second launch would bring this instance's window back (the single-instance
/// listener runs).
static RELAUNCH_SHOWS_WINDOW: AtomicBool = AtomicBool::new(false);

/// Whether opening Truffle again brings this window back, also from hidden. The UI hides the window
/// on closing only when there is such a way back (this, or the tray icon).
pub fn relaunch_shows_window() -> bool {
    RELAUNCH_SHOWS_WINDOW.load(Ordering::Relaxed)
}

/// True once after another launch asked Truffle to come to the front. The window has already
/// been asked to show, restore and focus itself by then; the UI uses this to sync its own state
/// (to leave its "hidden in the tray" mode).
pub fn take_activation_request() -> bool {
    ACTIVATION_REQUESTED.swap(false, Ordering::Relaxed)
}

/// How the main window appears at start-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartMode {
    Normal,
    /// `--minimized` without a tray icon: minimised to the taskbar.
    Minimized,
    /// `--minimized` with the tray icon on: no window and no taskbar button, only the tray icon.
    Hidden,
}

impl StartMode {
    fn new(minimized_arg: bool, tray_icon: bool) -> Self {
        match (minimized_arg, tray_icon) {
            (false, _) => Self::Normal,
            (true, true) => Self::Hidden,
            (true, false) => Self::Minimized,
        }
    }

    /// Call on the still-hidden window before the first frame. eframe paints (and with that
    /// shows) the window only while it isn't minimised, so minimising it first, without
    /// activating it, keeps eframe from showing it. For `Hidden` it is then hidden again; it
    /// stays minimised and hidden until the UI sends `ViewportCommand::Visible(true)` +
    /// `Minimized(false)` + `Focus` (e.g. on a tray click). Until then only `App::logic` runs.
    fn apply(self, hwnd: isize) {
        match self {
            Self::Normal => {}
            Self::Minimized => win32::minimize_without_activating(hwnd),
            Self::Hidden => {
                win32::minimize_without_activating(hwnd);
                win32::hide(hwnd);
            }
        }
        if self != Self::Normal {
            log::info!("started {self:?}");
        }
    }
}

/// One Truffle per Windows session.
///
/// The first instance owns a named mutex for its whole lifetime and listens on a named event
/// while its window exists. A later launch that finds the mutex taken signals the event (the
/// first instance then shows, restores and focuses its window) and exits. If the event is gone,
/// the first instance is shutting down: the new launch waits for it to exit, then starts
/// normally. Debug builds use their own names, so a dev build runs next to an installed one.
mod single_instance {
    use std::sync::atomic::Ordering;
    use std::thread::JoinHandle;
    use std::time::Duration;

    use crate::win32::{self, Handle, Wait};

    #[cfg(debug_assertions)]
    const NAMES: Names = Names {
        mutex: r"Local\Truffle-7d3f2a91-instance-dev",
        activate: r"Local\Truffle-7d3f2a91-activate-dev",
    };
    #[cfg(not(debug_assertions))]
    const NAMES: Names = Names {
        mutex: r"Local\Truffle-7d3f2a91-instance",
        activate: r"Local\Truffle-7d3f2a91-activate",
    };

    /// How long a new launch waits for a closing Truffle to finish saving and exit.
    const WAIT_FOR_EXIT: Duration = Duration::from_secs(15);

    struct Names {
        mutex: &'static str,
        activate: &'static str,
    }

    /// Held by the running instance. Drop it on the thread that acquired it (the main thread).
    pub struct Instance {
        mutex: Option<Handle>,
        activate: Option<Handle>,
        stop: Option<Handle>,
        listener: Option<JoinHandle<()>>,
    }

    impl Instance {
        /// `None`: another Truffle is running (and was asked to come to the front if
        /// `activate_existing`), so this process should exit. Never blocks start-up because of an
        /// OS error: if the named objects can't be created, Truffle just runs without the check.
        pub fn acquire(activate_existing: bool) -> Option<Self> {
            acquire_with(&NAMES, activate_existing, WAIT_FOR_EXIT)
        }

        /// Start answering later launches by showing the main window. Call once the UI exists.
        pub fn listen(&mut self, ctx: egui::Context) {
            let (Some(activate), Some(stop), None) = (self.activate, self.stop, &self.listener) else {
                return;
            };
            let spawned = std::thread::Builder::new()
                .name("truffle-activate".into())
                .stack_size(256 * 1024)
                .spawn(move || {
                    // `stop` first: if both are signalled, stopping wins.
                    while let Wait::Signalled(1) = win32::wait_any(&[stop, activate]) {
                        bring_to_front(&ctx);
                    }
                });
            match spawned {
                Ok(listener) => {
                    self.listener = Some(listener);
                    crate::RELAUNCH_SHOWS_WINDOW.store(true, Ordering::Relaxed);
                }
                Err(e) => log::warn!("could not start the single-instance listener: {e}"),
            }
        }

        /// Stop answering later launches; they then wait for this process to exit.
        pub fn stop_listening(&mut self) {
            if let Some(listener) = self.listener.take() {
                crate::RELAUNCH_SHOWS_WINDOW.store(false, Ordering::Relaxed);
                if let Some(stop) = self.stop {
                    win32::set_event(stop);
                }
                if listener.join().is_err() {
                    log::warn!("the single-instance listener stopped unexpectedly");
                }
            }
            if let Some(activate) = self.activate.take() {
                win32::close(activate);
            }
        }
    }

    impl Drop for Instance {
        fn drop(&mut self) {
            self.stop_listening();
            if let Some(stop) = self.stop.take() {
                win32::close(stop);
            }
            if let Some(mutex) = self.mutex.take() {
                win32::release_mutex(mutex);
                win32::close(mutex);
            }
        }
    }

    fn acquire_with(names: &Names, activate_existing: bool, wait_for_exit: Duration) -> Option<Instance> {
        // The event is created before the mutex, so whenever a running instance owns the mutex its
        // event exists too, unless it is shutting down.
        let Some((activate, event_existed)) = win32::create_event(Some(names.activate), false) else {
            log::warn!("single-instance check unavailable (event): {}", win32::last_error());
            return Some(Instance { mutex: None, activate: None, stop: None, listener: None });
        };
        let stop = win32::create_event(None, true).map(|(h, _)| h);
        let instance = |mutex| Instance { mutex, activate: Some(activate), stop, listener: None };

        let Some((mutex, mutex_existed)) = win32::create_mutex(names.mutex) else {
            log::warn!("single-instance check unavailable (mutex): {}", win32::last_error());
            return Some(instance(None));
        };
        if !mutex_existed {
            return Some(instance(Some(mutex)));
        }

        let mut other = instance(None);
        if event_existed {
            // The other instance is running (or starting) and listening.
            if activate_existing {
                win32::allow_any_process_to_take_focus();
                win32::set_event(activate);
            }
            win32::close(mutex);
            return None;
        }

        // We created the event, so the other instance has stopped listening: it is shutting down.
        log::info!("waiting for the previous Truffle to finish closing");
        match win32::wait_one(mutex, wait_for_exit) {
            Wait::Signalled(_) | Wait::Abandoned => {
                other.mutex = Some(mutex);
                Some(other)
            }
            Wait::Timeout | Wait::Failed => {
                log::warn!("the previous Truffle is still running after {wait_for_exit:?}");
                if activate_existing {
                    // In case it did come back to life: let it show itself.
                    win32::allow_any_process_to_take_focus();
                    win32::set_event(activate);
                }
                win32::close(mutex);
                None
            }
        }
    }

    fn bring_to_front(ctx: &egui::Context) {
        log::info!("another launch asked Truffle to come to the front");
        crate::ACTIVATION_REQUESTED.store(true, Ordering::Relaxed);
        let root = egui::ViewportId::ROOT;
        ctx.send_viewport_cmd_to(root, egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd_to(root, egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd_to(root, egui::ViewportCommand::Focus);
    }

    #[cfg(all(test, windows))]
    mod tests {
        use super::*;

        fn names(tag: &'static str) -> Names {
            // Leaked on purpose: tests need &'static names unique per test and process.
            let pid = std::process::id();
            Names {
                mutex: Box::leak(format!(r"Local\Truffle-test-{tag}-{pid}-mutex").into_boxed_str()),
                activate: Box::leak(format!(r"Local\Truffle-test-{tag}-{pid}-activate").into_boxed_str()),
            }
        }

        #[test]
        fn second_launch_signals_the_first_and_exits() {
            let names = names("signal");
            let first = acquire_with(&names, true, Duration::from_secs(1)).expect("first instance");
            assert!(first.mutex.is_some());
            let activate = first.activate.expect("event");
            assert!(matches!(win32::wait_one(activate, Duration::ZERO), Wait::Timeout));

            assert!(acquire_with(&names, true, Duration::from_secs(1)).is_none());
            assert!(matches!(win32::wait_one(activate, Duration::ZERO), Wait::Signalled(0)));

            // A start-with-Windows launch leaves quietly without signalling.
            assert!(acquire_with(&names, false, Duration::from_secs(1)).is_none());
            assert!(matches!(win32::wait_one(activate, Duration::ZERO), Wait::Timeout));
        }

        #[test]
        fn launch_during_shutdown_waits_and_takes_over() {
            let names = names("handover");
            let mut first = acquire_with(&names, true, Duration::from_secs(1)).expect("first instance");
            first.stop_listening();

            let second = std::thread::spawn(move || {
                let second = acquire_with(&names, true, Duration::from_secs(10));
                let took_over = second.as_ref().is_some_and(|s| s.mutex.is_some() && s.activate.is_some());
                drop(second);
                took_over
            });
            std::thread::sleep(Duration::from_millis(200));
            drop(first);
            assert!(second.join().expect("second thread"), "second launch took over");
        }

        #[test]
        fn launch_gives_up_when_the_old_instance_hangs() {
            let names = names("timeout");
            let mut first = acquire_with(&names, true, Duration::from_secs(1)).expect("first instance");
            first.stop_listening();
            let second = std::thread::spawn(move || acquire_with(&names, true, Duration::from_millis(100)).is_none());
            assert!(second.join().expect("second thread"));
            drop(first);
        }
    }
}

/// The few Win32 calls Truffle's start-up needs (declared here so they don't depend on which
/// `windows-sys` features other modules enable).
#[cfg(windows)]
mod win32 {
    use std::ffi::c_void;
    use std::time::Duration;

    /// A kernel object handle. Handles are process-wide, so they may cross threads.
    #[derive(Debug, Clone, Copy)]
    #[repr(transparent)]
    pub struct Handle(*mut c_void);
    // SAFETY: a HANDLE is an opaque process-wide value; the kernel synchronises access.
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}

    type Hwnd = *mut c_void;
    /// WNDENUMPROC (nullable, as in the C API and in `ui::window`'s declaration).
    type EnumProc = Option<unsafe extern "system" fn(Hwnd, isize) -> i32>;

    const ERROR_ALREADY_EXISTS: u32 = 183;
    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_ABANDONED_0: u32 = 0x80;
    const WAIT_TIMEOUT: u32 = 0x102;
    const INFINITE: u32 = u32::MAX;
    const ASFW_ANY: u32 = u32::MAX;
    const SW_HIDE: i32 = 0;
    const SW_SHOWMINNOACTIVE: i32 = 7;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateMutexW(attributes: *const c_void, initial_owner: i32, name: *const u16) -> Handle;
        fn ReleaseMutex(mutex: Handle) -> i32;
        fn CreateEventW(attributes: *const c_void, manual_reset: i32, initial_state: i32, name: *const u16) -> Handle;
        fn SetEvent(event: Handle) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
        fn WaitForSingleObject(handle: Handle, millis: u32) -> u32;
        fn WaitForMultipleObjects(count: u32, handles: *const Handle, wait_all: i32, millis: u32) -> u32;
        fn GetLastError() -> u32;
        fn GetCurrentThreadId() -> u32;
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn AllowSetForegroundWindow(process_id: u32) -> i32;
        fn EnumThreadWindows(thread_id: u32, callback: EnumProc, lparam: isize) -> i32;
        fn InternalGetWindowText(hwnd: Hwnd, text: *mut u16, max_count: i32) -> i32;
        fn ShowWindow(hwnd: Hwnd, cmd: i32) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn last_error() -> std::io::Error {
        std::io::Error::last_os_error()
    }

    /// Creates or opens a named mutex, owned by the calling thread if it was created.
    /// Returns the handle and whether it already existed.
    pub fn create_mutex(name: &str) -> Option<(Handle, bool)> {
        let name = wide(name);
        // SAFETY: valid NUL-terminated name, null attributes.
        let handle = unsafe { CreateMutexW(std::ptr::null(), 1, name.as_ptr()) };
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        (!handle.0.is_null()).then_some((handle, existed))
    }

    /// Creates or opens an event (unnamed if `name` is `None`), initially not signalled.
    /// Returns the handle and whether it already existed.
    pub fn create_event(name: Option<&str>, manual_reset: bool) -> Option<(Handle, bool)> {
        let name = name.map(wide);
        let name_ptr = name.as_ref().map_or(std::ptr::null(), |n| n.as_ptr());
        // SAFETY: valid NUL-terminated name or null, null attributes.
        let handle = unsafe { CreateEventW(std::ptr::null(), manual_reset as i32, 0, name_ptr) };
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        (!handle.0.is_null()).then_some((handle, existed))
    }

    pub fn set_event(event: Handle) {
        // SAFETY: plain call on a handle we own.
        if unsafe { SetEvent(event) } == 0 {
            log::warn!("SetEvent failed: {}", last_error());
        }
    }

    pub fn release_mutex(mutex: Handle) {
        // Fails harmlessly (ERROR_NOT_OWNER) if this thread doesn't own it.
        // SAFETY: plain call on a handle we own.
        unsafe { ReleaseMutex(mutex) };
    }

    pub fn close(handle: Handle) {
        // SAFETY: each handle is closed exactly once by its owner.
        unsafe { CloseHandle(handle) };
    }

    #[derive(Debug)]
    pub enum Wait {
        /// Index of the signalled handle.
        Signalled(usize),
        /// A mutex whose owner exited without releasing it (now owned by us).
        Abandoned,
        Timeout,
        Failed,
    }

    fn wait_result(code: u32, count: u32) -> Wait {
        match code {
            c if c < WAIT_OBJECT_0 + count => Wait::Signalled((c - WAIT_OBJECT_0) as usize),
            c if (WAIT_ABANDONED_0..WAIT_ABANDONED_0 + count).contains(&c) => Wait::Abandoned,
            WAIT_TIMEOUT => Wait::Timeout,
            _ => Wait::Failed,
        }
    }

    pub fn wait_one(handle: Handle, timeout: Duration) -> Wait {
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(INFINITE - 1);
        // SAFETY: plain call on a handle we own.
        wait_result(unsafe { WaitForSingleObject(handle, millis) }, 1)
    }

    /// Waits without a timeout until one of `handles` is signalled.
    pub fn wait_any(handles: &[Handle]) -> Wait {
        let count = handles.len() as u32;
        // SAFETY: the slice outlives the call; handles are owned by the caller.
        wait_result(unsafe { WaitForMultipleObjects(count, handles.as_ptr(), 0, INFINITE) }, count)
    }

    /// Lets the process we are about to signal bring its window to the foreground.
    pub fn allow_any_process_to_take_focus() {
        // SAFETY: no pointers involved.
        unsafe { AllowSetForegroundWindow(ASFW_ANY) };
    }

    /// A top-level window of the calling thread with exactly this title. Reads titles without
    /// sending window messages, so it is safe to call from inside the window's own callbacks.
    pub fn find_own_window(title: &str) -> Option<isize> {
        struct Search {
            title: Vec<u16>,
            found: Option<isize>,
        }
        unsafe extern "system" fn visit(hwnd: Hwnd, lparam: isize) -> i32 {
            // SAFETY: lparam is the `Search` below, alive for the whole enumeration.
            let search = unsafe { &mut *(lparam as *mut Search) };
            let mut buf = [0u16; 128];
            // SAFETY: the buffer length is passed along.
            let len = unsafe { InternalGetWindowText(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
            if len > 0 && buf[..len as usize] == search.title[..] {
                search.found = Some(hwnd as isize);
                return 0; // stop
            }
            1
        }
        let mut search = Search { title: title.encode_utf16().collect(), found: None };
        // SAFETY: the callback only touches `search`, which outlives the call.
        unsafe { EnumThreadWindows(GetCurrentThreadId(), Some(visit), &mut search as *mut Search as isize) };
        search.found
    }

    pub fn minimize_without_activating(hwnd: isize) {
        // SAFETY: a window handle of this thread; ShowWindow tolerates stale handles.
        unsafe { ShowWindow(hwnd as Hwnd, SW_SHOWMINNOACTIVE) };
    }

    pub fn hide(hwnd: isize) {
        // SAFETY: as above.
        unsafe { ShowWindow(hwnd as Hwnd, SW_HIDE) };
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn wait_codes() {
            assert!(matches!(wait_result(0, 2), Wait::Signalled(0)));
            assert!(matches!(wait_result(1, 2), Wait::Signalled(1)));
            assert!(matches!(wait_result(0x80, 1), Wait::Abandoned));
            assert!(matches!(wait_result(0x102, 1), Wait::Timeout));
            assert!(matches!(wait_result(u32::MAX, 1), Wait::Failed));
        }

        #[test]
        fn wide_is_nul_terminated() {
            assert_eq!(wide("Ab"), [65, 98, 0]);
        }

        #[test]
        fn no_window_with_an_unknown_title() {
            assert_eq!(find_own_window("no such window 1f3a"), None);
        }
    }
}

/// Non-Windows builds: no single-instance check, no start-up window tricks.
#[cfg(not(windows))]
mod win32 {
    use std::time::Duration;

    #[derive(Debug, Clone, Copy)]
    pub struct Handle;

    #[derive(Debug)]
    pub enum Wait {
        Signalled(usize),
        Abandoned,
        Timeout,
        Failed,
    }

    pub fn last_error() -> std::io::Error {
        std::io::Error::other("not supported on this platform")
    }
    pub fn create_mutex(_name: &str) -> Option<(Handle, bool)> {
        None
    }
    pub fn create_event(_name: Option<&str>, _manual_reset: bool) -> Option<(Handle, bool)> {
        None
    }
    pub fn set_event(_event: Handle) {}
    pub fn release_mutex(_mutex: Handle) {}
    pub fn close(_handle: Handle) {}
    pub fn wait_one(_handle: Handle, _timeout: Duration) -> Wait {
        Wait::Failed
    }
    pub fn wait_any(_handles: &[Handle]) -> Wait {
        Wait::Failed
    }
    pub fn allow_any_process_to_take_focus() {}
    pub fn find_own_window(_title: &str) -> Option<isize> {
        None
    }
    pub fn minimize_without_activating(_hwnd: isize) {}
    pub fn hide(_hwnd: isize) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_mode_from_flag_and_tray() {
        assert_eq!(StartMode::new(false, true), StartMode::Normal);
        assert_eq!(StartMode::new(false, false), StartMode::Normal);
        assert_eq!(StartMode::new(true, true), StartMode::Hidden);
        assert_eq!(StartMode::new(true, false), StartMode::Minimized);
    }
}
