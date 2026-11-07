//! Native window helpers: finding the main window's handle (for the Windows media overlay) and
//! checking that a remembered position is still on a connected screen, which screen the window is
//! on (each screen has its own interface size, `screens.rs`), plus the pure geometry
//! rules of the mini player; and for the Settings window (`settings.rs`), the pure rules of where
//! it goes beside the main window (as in Glint) and the few calls that put it there. A close sent
//! from outside to winit's hidden helper window is passed on to the main window.
//!
//! eframe exposes the handle only through the `raw-window-handle` traits, and that crate is not a
//! direct dependency, so the handle is found with two plain user32 calls instead: the main window
//! is the top-level winit window ("Window Class") created on this (the UI) thread.

use egui::{Vec2, vec2};

/// Title of the main window (`main.rs` creates the window with it).
pub const WINDOW_TITLE: &str = "Truffle";

/// Size of the mini player, in points (it scales with the interface size like everything else).
pub const MINI_SIZE: Vec2 = vec2(380.0, 120.0);
/// Smallest size of the full window, in logical pixels (`main.rs` creates the window with it).
pub const FULL_MIN_SIZE: Vec2 = vec2(400.0, 480.0);
/// Size of the full window when nothing was remembered, in logical pixels.
pub const FULL_DEFAULT_SIZE: Vec2 = vec2(1180.0, 760.0);
/// The Now playing view's window the first time, in logical pixels: the design's 16:10, about 1104 x 690
/// at 100 % scaling. It keeps that shape; only its corners size it.
pub const STAGE_SIZE: Vec2 = vec2(1104.0, 690.0);
/// The smallest the view's window gets: about where the view stops shrinking its sizes (600 points
/// at the interface's usual 125 %).
pub const STAGE_MIN_SIZE: Vec2 = vec2(768.0, 480.0);

/// A remembered size above this is treated as damaged and capped (eframe also clamps to the
/// monitor's size).
const FULL_MAX_SIDE: f32 = 16_384.0;

/// A remembered full-window size (logical pixels): too small grows to the minimum, absurd is
/// capped, missing or damaged (not a positive number) gives the default. Used both for the size
/// the window opens at (`main.rs`) and when leaving the mini player.
pub fn full_size_or_default(saved: Option<[f32; 2]>) -> Vec2 {
    match saved {
        Some([w, h]) if w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0 => {
            vec2(w.clamp(FULL_MIN_SIZE.x, FULL_MAX_SIDE), h.clamp(FULL_MIN_SIZE.y, FULL_MAX_SIDE))
        }
        _ => FULL_DEFAULT_SIZE,
    }
}

/// The Now playing view's window size (logical pixels, 16:10): as it was left, else [`STAGE_SIZE`];
/// never under [`STAGE_MIN_SIZE`] nor past `screen` (its size, a little in). The height follows the
/// width.
pub fn stage_size_or_default(saved: Option<[f32; 2]>, screen: Option<Vec2>) -> Vec2 {
    let ratio = STAGE_SIZE.x / STAGE_SIZE.y;
    let w = match saved {
        Some([w, _]) if w.is_finite() && w > 0.0 => w,
        _ => STAGE_SIZE.x,
    };
    let most = screen.filter(|s| s.x > 0.0 && s.y > 0.0).map_or(FULL_MAX_SIDE, |s| (s.x * 0.9).min(s.y * 0.9 * ratio));
    let w = w.min(most).max(STAGE_MIN_SIZE.x).round();
    vec2(w, (w / ratio).round())
}

/// While a window is sized by `edge` (Windows' WMSZ_: 1 left, 2 right, 3 top, 4 top left, 5 top
/// right, 6 bottom, 7 bottom left, 8 bottom right), the rect `to` Windows proposes (left, top, right,
/// bottom; screen pixels), changed so its inside (less `frame` across and down) keeps `ratio` and is
/// at least `min_w` wide. From a corner the size follows whichever way the mouse went further since
/// the drag began (`was`: Windows makes each proposal from the cursor and that start, not from what
/// we made of the last one); from a side (the keyboard's Size) the other side comes along. The side
/// or corner across stays put.
pub fn keep_ratio(to: [i32; 4], was: [i32; 4], edge: u32, ratio: f32, frame: [i32; 2], min_w: i32) -> [i32; 4] {
    let [left, top, right, bottom] = to;
    let inside = |r: [i32; 4]| ((r[2] - r[0] - frame[0]) as f32, (r[3] - r[1] - frame[1]) as f32);
    let ((w, h), (w0, h0)) = (inside(to), inside(was));
    let w = match edge {
        1 | 2 => w,
        3 | 6 => h * ratio,
        _ if (w - w0).abs() >= (h - h0).abs() * ratio => w,
        _ => h * ratio,
    };
    let w = w.max(min_w as f32).round();
    let (ww, wh) = (w as i32 + frame[0], (w / ratio).round() as i32 + frame[1]);
    let (left, right) = if matches!(edge, 1 | 4 | 7) { (right - ww, right) } else { (left, left + ww) };
    let (top, bottom) = if matches!(edge, 3..=5) { (bottom - wh, bottom) } else { (top, top + wh) };
    [left, top, right, bottom]
}

/// While the Now playing view is up the main window keeps the shape `ratio` (width / height) as it
/// is sized, at least `min_w` logical pixels wide (on any screen); `None` lets it go. Call on the
/// UI thread.
pub fn keep_shape(main: isize, ratio: Option<f32>, min_w: i32) {
    #[cfg(windows)]
    {
        if !win32::keep_shape(main, ratio, min_w) {
            log::warn!("could not keep the Now playing view's shape");
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (main, ratio, min_w);
    }
}

/// This process's memory as Windows counts it: private (committed) bytes, the working set, and the
/// working set's peak.
pub fn process_memory() -> Option<(usize, usize, usize)> {
    #[cfg(windows)]
    {
        win32::process_memory()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// A remembered screen position (physical pixels) that is a real number in a plausible range.
pub fn plausible_position(pos: Option<[f32; 2]>) -> Option<[f32; 2]> {
    pos.filter(|p| p.iter().all(|v| v.is_finite() && v.abs() < 100_000.0))
}

/// Whether two remembered values differ enough to be worth saving again.
pub fn moved(old: Option<[f32; 2]>, new: [f32; 2]) -> bool {
    old.is_none_or(|o| (o[0] - new[0]).abs() > 0.5 || (o[1] - new[1]).abs() > 0.5)
}

/// A rectangle on the screen in physical pixels; `right` and `bottom` are just outside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl ScreenRect {
    pub fn width(self) -> i32 {
        self.right - self.left
    }

    pub fn height(self) -> i32 {
        self.bottom - self.top
    }
}

/// The Settings window: `w` x `h` centred over `main`, moved (and shrunk) to fit on its screen
/// (`work`).
pub fn centred_over(main: ScreenRect, work: ScreenRect, w: i32, h: i32) -> ScreenRect {
    let (w, h) = (w.min(work.width()).max(1), h.min(work.height()).max(1));
    let x = ((main.left + main.right) / 2 - w / 2).min(work.right - w).max(work.left);
    let y = ((main.top + main.bottom) / 2 - h / 2).min(work.bottom - h).max(work.top);
    ScreenRect { left: x, top: y, right: x + w, bottom: y + h }
}

/// The main window's handle (HWND) as an integer, if it can be found.
pub fn main_window_handle() -> Option<isize> {
    #[cfg(windows)]
    {
        win32::find_main_window(WINDOW_TITLE)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// A window of this program, made by winit on this thread, with exactly this title.
pub fn find_window(title: &str) -> Option<isize> {
    #[cfg(windows)]
    {
        win32::find_window(title)
    }
    #[cfg(not(windows))]
    {
        let _ = title;
        None
    }
}

/// Makes a close sent to winit's hidden helper window close `main` (the main window) instead.
///
/// winit gives the UI thread an invisible helper window and marks it visible (it needs paint
/// messages). Tools that close a program's "main window" from outside, such as PowerShell's
/// `CloseMainWindow`, take the first visible one, and while Truffle is hidden in the tray that is
/// the helper. Windows' default answer to a close destroys it: egui never hears of the close and
/// Truffle keeps running. Call once, on the UI thread.
pub fn forward_helper_close(main: isize) {
    #[cfg(windows)]
    {
        if !win32::forward_helper_close(main) {
            log::warn!("could not watch winit's helper window; a close from outside may be missed");
        }
    }
    #[cfg(not(windows))]
    {
        let _ = main;
    }
}

/// The part of a window you can see (Windows 10 and 11 add invisible borders round a window).
pub fn visible_rect(hwnd: isize) -> Option<ScreenRect> {
    #[cfg(windows)]
    {
        win32::visible_rect(hwnd)
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        None
    }
}

/// The work area (the screen without the taskbar) of the screen a window is mostly on.
pub fn work_area(hwnd: isize) -> Option<ScreenRect> {
    #[cfg(windows)]
    {
        win32::work_area(hwnd)
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        None
    }
}

/// Where a window goes back to when it isn't maximized or minimised (Windows' "normal" place), and
/// whether it is (or will come back) maximized.
pub fn placement(hwnd: isize) -> Option<(ScreenRect, bool)> {
    #[cfg(windows)]
    {
        win32::placement(hwnd)
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        None
    }
}

/// Puts a still hidden window at its remembered normal place, if that is on a connected screen
/// (it stays hidden). Returns whether it did.
pub fn restore_placement(hwnd: isize, normal: ScreenRect) -> bool {
    #[cfg(windows)]
    {
        win32::restore_placement(hwnd, normal)
    }
    #[cfg(not(windows))]
    {
        let _ = (hwnd, normal);
        false
    }
}

/// Whether a window is minimised.
pub fn is_minimized(hwnd: isize) -> bool {
    #[cfg(windows)]
    {
        win32::is_minimized(hwnd)
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        false
    }
}

/// Moves `hwnd` so the part of it you can see is `to`, its invisible borders outside it. Doesn't
/// activate it or change which window is in front.
pub fn place_visible(hwnd: isize, to: ScreenRect) {
    #[cfg(windows)]
    {
        win32::place_visible(hwnd, to);
    }
    #[cfg(not(windows))]
    {
        let _ = (hwnd, to);
    }
}

/// Makes `owner` the owner of `hwnd`, as Glint's Settings window is: it stays in front of the
/// owner, is minimised with it and has no taskbar button.
pub fn set_owner(hwnd: isize, owner: isize) {
    #[cfg(windows)]
    {
        win32::set_owner(hwnd, owner);
    }
    #[cfg(not(windows))]
    {
        let _ = (hwnd, owner);
    }
}

/// Shows a window made hidden, still cloaked (Windows counts it as shown, so it can be drawn, but
/// nothing is seen yet): without Windows' opening animation, and without taking the focus when
/// `activate` is false. [`uncloak`] it once it has drawn itself: a window shown straight away
/// shows white until its first picture.
pub fn show_cloaked(hwnd: isize, activate: bool) {
    #[cfg(windows)]
    {
        win32::show_cloaked(hwnd, activate);
    }
    #[cfg(not(windows))]
    {
        let _ = (hwnd, activate);
    }
}

/// Lets a window shown with [`show_cloaked`] be seen.
pub fn uncloak(hwnd: isize) {
    #[cfg(windows)]
    {
        win32::uncloak(hwnd);
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
    }
}

/// A connected screen: Windows' name for it (`\\.\DISPLAY1`), its size in physical pixels and
/// Windows' scale on it (1.0 at 100%).
#[derive(Clone, Debug, PartialEq)]
pub struct Monitor {
    pub name: String,
    pub width: i32,
    pub height: i32,
    pub scale: f32,
}

/// The screen a window is mostly on.
pub fn monitor_of(hwnd: isize) -> Option<Monitor> {
    #[cfg(windows)]
    {
        win32::monitor_of(hwnd)
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        None
    }
}

/// The screen under the physical screen point `(x, y)`, or the nearest one.
pub fn monitor_at(x: i32, y: i32) -> Option<Monitor> {
    #[cfg(windows)]
    {
        win32::monitor_at(x, y)
    }
    #[cfg(not(windows))]
    {
        let _ = (x, y);
        None
    }
}

/// Whether the physical screen point `(x, y)` is on a connected monitor. Unknown (non-Windows) →
/// true.
pub fn point_on_a_monitor(x: f32, y: f32) -> bool {
    #[cfg(windows)]
    {
        win32::point_on_a_monitor(x.round() as i32, y.round() as i32)
    }
    #[cfg(not(windows))]
    {
        let _ = (x, y);
        true
    }
}

#[cfg(windows)]
mod win32 {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

    use super::{Monitor, ScreenRect};

    type Hwnd = *mut c_void;

    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    struct WindowPlacement {
        length: u32,
        flags: u32,
        show_cmd: u32,
        min_position: Point,
        max_position: Point,
        normal_position: Rect,
    }

    const SW_HIDE: u32 = 0;
    const SW_SHOWMINIMIZED: u32 = 2;
    const SW_SHOWMAXIMIZED: u32 = 3;
    const WPF_RESTORETOMAXIMIZED: u32 = 2;

    #[repr(C)]
    struct MonitorInfo {
        size: u32,
        monitor: Rect,
        work: Rect,
        flags: u32,
    }

    /// `MONITORINFOEXW`: the above and the monitor's name.
    #[repr(C)]
    struct MonitorInfoEx {
        info: MonitorInfo,
        device: [u16; 32],
    }

    const MONITOR_DEFAULTTONULL: u32 = 0;
    const MDT_EFFECTIVE_DPI: i32 = 0;
    const MONITOR_DEFAULTTONEAREST: u32 = 2;
    const DWMWA_EXTENDED_FRAME_BOUNDS: u32 = 9;
    const SWP_NOZORDER: u32 = 0x0004;
    const SWP_NOACTIVATE: u32 = 0x0010;
    const SWP_NOOWNERZORDER: u32 = 0x0200;
    const GWLP_HWNDPARENT: i32 = -8;
    const DWMWA_TRANSITIONS_FORCEDISABLED: u32 = 3;
    const DWMWA_CLOAK: u32 = 13;
    const SW_SHOW: i32 = 5;
    const SW_SHOWNOACTIVATE: i32 = 4;
    /// winit's default window class name on Windows.
    const WINIT_CLASS: &str = "Window Class";
    /// The class of winit's hidden helper window (one per UI thread).
    const WINIT_HELPER_CLASS: &str = "Winit Thread Event Target";
    const WM_CLOSE: u32 = 0x0010;
    const WM_NCDESTROY: u32 = 0x0082;
    const WM_SIZING: u32 = 0x0214;
    const WM_ENTERSIZEMOVE: u32 = 0x0231;
    const WM_EXITSIZEMOVE: u32 = 0x0232;
    /// Tells Truffle's subclass of the helper apart from any other on it.
    const HELPER_SUBCLASS_ID: usize = 1;
    /// Tells Truffle's subclass of the main window (keeping the view's shape) apart.
    const SHAPE_SUBCLASS_ID: usize = 2;
    /// The shape the main window keeps while it is sized (width / height as f32 bits; 0: none), and
    /// its least width in logical pixels.
    static SHAPE: AtomicU32 = AtomicU32::new(0);
    static SHAPE_MIN_W: AtomicI32 = AtomicI32::new(0);
    thread_local! {
        /// The window's rect when the user began sizing it (the window's own thread only).
        static SIZING_FROM: std::cell::Cell<Option<Rect>> = const { std::cell::Cell::new(None) };
    }

    /// SUBCLASSPROC: window, message, wparam, lparam, subclass id, its data.
    type SubclassProc = unsafe extern "system" fn(Hwnd, u32, usize, isize, usize, usize) -> isize;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn EnumThreadWindows(thread_id: u32, callback: Option<unsafe extern "system" fn(Hwnd, isize) -> i32>, lparam: isize) -> i32;
        fn GetClassNameW(hwnd: Hwnd, name: *mut u16, max_count: i32) -> i32;
        fn InternalGetWindowText(hwnd: Hwnd, text: *mut u16, max_count: i32) -> i32;
        fn MonitorFromPoint(point: Point, flags: u32) -> *mut c_void;
        fn MonitorFromWindow(hwnd: Hwnd, flags: u32) -> *mut c_void;
        fn GetMonitorInfoW(monitor: *mut c_void, info: *mut MonitorInfo) -> i32;
        fn MonitorFromRect(rect: *const Rect, flags: u32) -> *mut c_void;
        fn GetWindowPlacement(hwnd: Hwnd, placement: *mut WindowPlacement) -> i32;
        fn SetWindowPlacement(hwnd: Hwnd, placement: *const WindowPlacement) -> i32;
        fn GetWindowRect(hwnd: Hwnd, rect: *mut Rect) -> i32;
        fn GetClientRect(hwnd: Hwnd, rect: *mut Rect) -> i32;
        fn GetDpiForWindow(hwnd: Hwnd) -> u32;
        fn SetWindowPos(hwnd: Hwnd, insert_after: Hwnd, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> i32;
        fn IsIconic(hwnd: Hwnd) -> i32;
        fn ShowWindow(hwnd: Hwnd, cmd: i32) -> i32;
        fn PostMessageW(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> i32;
        #[cfg(target_pointer_width = "64")]
        fn SetWindowLongPtrW(hwnd: Hwnd, index: i32, value: isize) -> isize;
        #[cfg(not(target_pointer_width = "64"))]
        fn SetWindowLongW(hwnd: Hwnd, index: i32, value: i32) -> i32;
    }

    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmGetWindowAttribute(hwnd: Hwnd, attribute: u32, value: *mut c_void, size: u32) -> i32;
        fn DwmSetWindowAttribute(hwnd: Hwnd, attribute: u32, value: *const c_void, size: u32) -> i32;
    }

    #[link(name = "shcore")]
    unsafe extern "system" {
        fn GetDpiForMonitor(monitor: *mut c_void, kind: i32, dpi_x: *mut u32, dpi_y: *mut u32) -> i32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
        fn GetCurrentProcess() -> *mut c_void;
        fn K32GetProcessMemoryInfo(process: *mut c_void, counters: *mut ProcessMemoryCounters, cb: u32) -> i32;
    }

    /// PROCESS_MEMORY_COUNTERS.
    #[repr(C)]
    #[derive(Default)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    /// (private bytes, working set, peak working set) of this process.
    pub fn process_memory() -> Option<(usize, usize, usize)> {
        let mut c = ProcessMemoryCounters { cb: size_of::<ProcessMemoryCounters>() as u32, ..Default::default() };
        // SAFETY: fills a struct of the size it is told, for this process.
        let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) } != 0;
        ok.then_some((c.pagefile_usage, c.working_set_size, c.peak_working_set_size))
    }

    #[link(name = "comctl32")]
    unsafe extern "system" {
        fn SetWindowSubclass(hwnd: Hwnd, subclass: Option<SubclassProc>, id: usize, data: usize) -> i32;
        fn RemoveWindowSubclass(hwnd: Hwnd, subclass: Option<SubclassProc>, id: usize) -> i32;
        fn DefSubclassProc(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize;
    }

    /// Windows of this thread with one window class: (handle, title).
    struct Found {
        class: &'static str,
        windows: Vec<(isize, String)>,
    }

    fn read_wide(f: impl FnOnce(*mut u16, i32) -> i32) -> String {
        let mut buf = [0u16; 256];
        let n = f(buf.as_mut_ptr(), buf.len() as i32);
        let n = usize::try_from(n).unwrap_or(0).min(buf.len());
        String::from_utf16_lossy(&buf[..n])
    }

    unsafe extern "system" fn collect(hwnd: Hwnd, lparam: isize) -> i32 {
        // SAFETY: `lparam` is the `&mut Found` passed to EnumThreadWindows below, which outlives
        // the enumeration; the handle comes straight from Windows and the buffers are local.
        let found = unsafe { &mut *(lparam as *mut Found) };
        let class = read_wide(|buf, len| unsafe { GetClassNameW(hwnd, buf, len) });
        if class == found.class {
            // Without sending the window a message: this also runs from inside its own events.
            let title = read_wide(|buf, len| unsafe { InternalGetWindowText(hwnd, buf, len) });
            found.windows.push((hwnd as isize, title));
        }
        1 // keep enumerating
    }

    /// This thread's windows of one class (winit's: [`WINIT_CLASS`]): (handle, title).
    fn own_windows(class: &'static str) -> Vec<(isize, String)> {
        let mut found = Found { class, windows: Vec::new() };
        // SAFETY: plain Win32 calls; the callback only touches `found`, which lives on this stack
        // frame for the whole (synchronous) enumeration.
        unsafe {
            EnumThreadWindows(GetCurrentThreadId(), Some(collect), &mut found as *mut Found as isize);
        }
        found.windows
    }

    pub fn find_window(title: &str) -> Option<isize> {
        own_windows(WINIT_CLASS).into_iter().find(|(_, t)| t == title).map(|(h, _)| h)
    }

    pub fn forward_helper_close(main: isize) -> bool {
        own_windows(WINIT_HELPER_CLASS).first().is_some_and(|&(helper, _)| hook_helper(helper, main))
    }

    /// Passes a close sent to `helper` on to `main`. Both must be windows of this thread.
    pub fn hook_helper(helper: isize, main: isize) -> bool {
        // SAFETY: subclasses a window of this thread (as SetWindowSubclass requires); `main` is
        // only data handed back to `helper_proc`, which never reads through it.
        unsafe { SetWindowSubclass(helper as Hwnd, Some(helper_proc), HELPER_SUBCLASS_ID, main as usize) != 0 }
    }

    /// Sees the helper's messages before winit does: a close is posted to the main window, as if it
    /// had been sent there (winit turns that into egui's close request), and the helper stays.
    unsafe extern "system" fn helper_proc(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize, _id: usize, main: usize) -> isize {
        if msg == WM_CLOSE {
            // SAFETY: posts to a window handle; a stale one just fails.
            unsafe { PostMessageW(main as Hwnd, WM_CLOSE, 0, 0) };
            return 0;
        }
        if msg == WM_NCDESTROY {
            // SAFETY: removes this subclass from its own window as that window goes away (as the
            // documentation asks), on the window's thread.
            unsafe { RemoveWindowSubclass(hwnd, Some(helper_proc), HELPER_SUBCLASS_ID) };
        }
        // SAFETY: hands the message on to the window's own procedure (winit's), unchanged.
        unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
    }

    /// Keeps `main`'s shape `ratio` while it is sized (`None`: lets it go), at least `min_w` logical
    /// pixels wide.
    pub fn keep_shape(main: isize, ratio: Option<f32>, min_w: i32) -> bool {
        SHAPE.store(ratio.map_or(0, f32::to_bits), Ordering::Relaxed);
        SHAPE_MIN_W.store(min_w, Ordering::Relaxed);
        // SAFETY: subclasses a window of this thread (as SetWindowSubclass requires); doing it again
        // when it is on just keeps it.
        unsafe { SetWindowSubclass(main as Hwnd, Some(shape_proc), SHAPE_SUBCLASS_ID, 0) != 0 }
    }

    /// Sees the main window's messages before winit does: while a shape is kept, each size Windows
    /// proposes as the window is sized is changed to keep it ([`super::keep_ratio`]).
    unsafe extern "system" fn shape_proc(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize, _id: usize, _data: usize) -> isize {
        let ratio = f32::from_bits(SHAPE.load(Ordering::Relaxed));
        if msg == WM_ENTERSIZEMOVE {
            let mut from = Rect::default();
            // SAFETY: reads this window's rect into a local.
            let read = unsafe { GetWindowRect(hwnd, &mut from) } != 0;
            SIZING_FROM.with(|s| s.set(read.then_some(from)));
        } else if msg == WM_EXITSIZEMOVE {
            SIZING_FROM.with(|s| s.set(None));
        }
        if msg == WM_SIZING && ratio > 0.0 && lparam != 0 {
            let (mut now, mut inside) = (Rect::default(), Rect::default());
            // SAFETY: reads this window's rects and scale into locals.
            let (read, dpi) = unsafe { (GetWindowRect(hwnd, &mut now) != 0 && GetClientRect(hwnd, &mut inside) != 0, GetDpiForWindow(hwnd)) };
            if read {
                let was = SIZING_FROM.with(std::cell::Cell::get).unwrap_or(now);
                // SAFETY: for WM_SIZING, lparam points to the RECT Windows proposes, there to be changed.
                let to = unsafe { &mut *(lparam as *mut Rect) };
                let frame = [(now.right - now.left) - (inside.right - inside.left), (now.bottom - now.top) - (inside.bottom - inside.top)];
                let scale = if dpi > 0 { dpi as f32 / 96.0 } else { 1.0 };
                let [left, top, right, bottom] = super::keep_ratio(
                    [to.left, to.top, to.right, to.bottom],
                    [was.left, was.top, was.right, was.bottom],
                    wparam as u32,
                    ratio,
                    frame,
                    (SHAPE_MIN_W.load(Ordering::Relaxed) as f32 * scale).round() as i32,
                );
                *to = Rect { left, top, right, bottom };
            }
        }
        if msg == WM_NCDESTROY {
            // SAFETY: removes this subclass from its own window as that window goes away (as the
            // documentation asks), on the window's thread.
            unsafe { RemoveWindowSubclass(hwnd, Some(shape_proc), SHAPE_SUBCLASS_ID) };
        }
        // SAFETY: hands the message on to the window's own procedure (winit's), the size changed.
        unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
    }

    #[cfg(test)]
    #[link(name = "user32")]
    unsafe extern "system" {
        fn CreateWindowExW(ex: u32, class: *const u16, name: *const u16, style: u32, x: i32, y: i32, w: i32, h: i32, parent: Hwnd, menu: *mut c_void, inst: *mut c_void, param: *const c_void) -> Hwnd;
        fn DestroyWindow(hwnd: Hwnd) -> i32;
        fn SendMessageW(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize;
    }

    /// A hidden plain window (the system's STATIC class, no frame) at `r`, for tests.
    #[cfg(test)]
    pub fn test_window(r: [i32; 4]) -> isize {
        const WS_POPUP: u32 = 0x8000_0000;
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        // SAFETY: creates a window of a system class on this thread; the strings outlive the call.
        unsafe { CreateWindowExW(0, class.as_ptr(), class.as_ptr(), WS_POPUP, r[0], r[1], r[2] - r[0], r[3] - r[1], std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null()) as isize }
    }

    /// What `hwnd` makes of the rect `to` while sized by `edge`, as in a drag (WM_SIZING), for tests.
    #[cfg(test)]
    pub fn sizing(hwnd: isize, edge: u32, to: [i32; 4]) -> [i32; 4] {
        let mut r = Rect { left: to[0], top: to[1], right: to[2], bottom: to[3] };
        // SAFETY: sends to a window of this thread; the rect lives through the (synchronous) call.
        unsafe { SendMessageW(hwnd as Hwnd, WM_SIZING, edge as usize, &mut r as *mut Rect as isize) };
        [r.left, r.top, r.right, r.bottom]
    }

    #[cfg(test)]
    pub fn destroy(hwnd: isize) {
        // SAFETY: destroys a window this thread made.
        unsafe { DestroyWindow(hwnd as Hwnd) };
    }

    pub fn find_main_window(title: &str) -> Option<isize> {
        let windows = own_windows(WINIT_CLASS);
        let by_title = windows.iter().find(|(_, t)| t == title).map(|(h, _)| *h);
        let only_one = (windows.len() == 1).then(|| windows[0].0);
        let hwnd = by_title.or(only_one);
        if hwnd.is_none() {
            log::warn!("could not find the main window handle ({} candidates)", windows.len());
        }
        hwnd
    }

    pub fn point_on_a_monitor(x: i32, y: i32) -> bool {
        // SAFETY: plain Win32 call with a value argument.
        !unsafe { MonitorFromPoint(Point { x, y }, MONITOR_DEFAULTTONULL) }.is_null()
    }

    fn screen_rect(r: Rect) -> Option<ScreenRect> {
        (r.right > r.left && r.bottom > r.top).then_some(ScreenRect { left: r.left, top: r.top, right: r.right, bottom: r.bottom })
    }

    fn window_rect(hwnd: isize) -> Option<Rect> {
        let mut r = Rect::default();
        // SAFETY: a plain query with a valid out-struct; a stale handle just fails.
        (unsafe { GetWindowRect(hwnd as Hwnd, &mut r) } != 0).then_some(r)
    }

    pub fn visible_rect(hwnd: isize) -> Option<ScreenRect> {
        let mut r = Rect::default();
        // SAFETY: a plain query with a valid out-struct of the size passed.
        let hr = unsafe {
            DwmGetWindowAttribute(hwnd as Hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, (&mut r as *mut Rect).cast(), size_of::<Rect>() as u32)
        };
        screen_rect(r).filter(|_| hr >= 0).or_else(|| window_rect(hwnd).and_then(screen_rect))
    }

    pub fn work_area(hwnd: isize) -> Option<ScreenRect> {
        let mut info = MonitorInfo { size: size_of::<MonitorInfo>() as u32, monitor: Rect::default(), work: Rect::default(), flags: 0 };
        // SAFETY: plain queries; `info` is valid and its size is set as the call requires.
        let ok = unsafe {
            let monitor = MonitorFromWindow(hwnd as Hwnd, MONITOR_DEFAULTTONEAREST);
            !monitor.is_null() && GetMonitorInfoW(monitor, &mut info) != 0
        };
        if ok { screen_rect(info.work) } else { None }
    }

    pub fn monitor_of(hwnd: isize) -> Option<Monitor> {
        // SAFETY: a plain query; a stale handle just gives null.
        monitor_info(unsafe { MonitorFromWindow(hwnd as Hwnd, MONITOR_DEFAULTTONEAREST) })
    }

    pub fn monitor_at(x: i32, y: i32) -> Option<Monitor> {
        // SAFETY: plain Win32 call with a value argument.
        monitor_info(unsafe { MonitorFromPoint(Point { x, y }, MONITOR_DEFAULTTONEAREST) })
    }

    fn monitor_info(monitor: *mut c_void) -> Option<Monitor> {
        let info = MonitorInfo { size: size_of::<MonitorInfoEx>() as u32, monitor: Rect::default(), work: Rect::default(), flags: 0 };
        let mut ex = MonitorInfoEx { info, device: [0; 32] };
        // SAFETY: plain queries on a monitor handle from Windows (null is checked); `ex` is valid
        // and its size is set as the call requires (the extended form, which adds the name), and
        // the DPI goes to two local numbers.
        let (mut dpi, mut dpi_y) = (0, 0);
        let ok = unsafe {
            let ok = !monitor.is_null() && GetMonitorInfoW(monitor, (&mut ex as *mut MonitorInfoEx).cast()) != 0;
            if ok && GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y) < 0 {
                dpi = 0;
            }
            ok
        };
        let r = screen_rect(ex.info.monitor).filter(|_| ok)?;
        let len = ex.device.iter().position(|&c| c == 0).unwrap_or(ex.device.len());
        let name = String::from_utf16_lossy(&ex.device[..len]);
        let scale = if dpi > 0 { dpi as f32 / 96.0 } else { 1.0 };
        Some(Monitor { name, width: r.width(), height: r.height(), scale })
    }

    fn empty_placement() -> WindowPlacement {
        WindowPlacement {
            length: size_of::<WindowPlacement>() as u32,
            flags: 0,
            show_cmd: 0,
            min_position: Point { x: 0, y: 0 },
            max_position: Point { x: 0, y: 0 },
            normal_position: Rect::default(),
        }
    }

    pub fn placement(hwnd: isize) -> Option<(ScreenRect, bool)> {
        let mut p = empty_placement();
        // SAFETY: a plain query with a valid out-struct whose length is set as the call requires.
        if unsafe { GetWindowPlacement(hwnd as Hwnd, &mut p) } == 0 {
            return None;
        }
        let maximized = p.show_cmd == SW_SHOWMAXIMIZED || (p.show_cmd == SW_SHOWMINIMIZED && p.flags & WPF_RESTORETOMAXIMIZED != 0);
        screen_rect(p.normal_position).map(|r| (r, maximized))
    }

    pub fn restore_placement(hwnd: isize, normal: ScreenRect) -> bool {
        let rect = Rect { left: normal.left, top: normal.top, right: normal.right, bottom: normal.bottom };
        // SAFETY: plain Win32 calls with valid structs; a screen that is gone gives null.
        unsafe {
            if MonitorFromRect(&rect, MONITOR_DEFAULTTONULL).is_null() {
                return false;
            }
            let mut p = empty_placement();
            p.show_cmd = SW_HIDE;
            p.normal_position = rect;
            SetWindowPlacement(hwnd as Hwnd, &p) != 0
        }
    }

    pub fn is_minimized(hwnd: isize) -> bool {
        // SAFETY: a plain query; a stale handle answers no.
        unsafe { IsIconic(hwnd as Hwnd) != 0 }
    }

    pub fn place_visible(hwnd: isize, to: ScreenRect) {
        let (Some(whole), Some(seen)) = (window_rect(hwnd), visible_rect(hwnd)) else { return };
        let (l, t) = (seen.left - whole.left, seen.top - whole.top);
        let (r, b) = (whole.right - seen.right, whole.bottom - seen.bottom);
        // SAFETY: moves a window of this thread; no pointers involved.
        unsafe {
            SetWindowPos(
                hwnd as Hwnd,
                std::ptr::null_mut(),
                to.left - l,
                to.top - t,
                to.width() + l + r,
                to.height() + t + b,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
            );
        }
    }

    fn set_dwm_bool(hwnd: isize, attribute: u32, on: bool) {
        let value = i32::from(on);
        // SAFETY: a plain call on a window of this thread; `value` outlives the call that reads it.
        unsafe {
            DwmSetWindowAttribute(hwnd as Hwnd, attribute, (&value as *const i32).cast(), size_of::<i32>() as u32);
        }
    }

    pub fn show_cloaked(hwnd: isize, activate: bool) {
        set_dwm_bool(hwnd, DWMWA_TRANSITIONS_FORCEDISABLED, true);
        set_dwm_bool(hwnd, DWMWA_CLOAK, true);
        // SAFETY: shows a window of this thread.
        unsafe {
            ShowWindow(hwnd as Hwnd, if activate { SW_SHOW } else { SW_SHOWNOACTIVATE });
        }
    }

    pub fn uncloak(hwnd: isize) {
        set_dwm_bool(hwnd, DWMWA_CLOAK, false);
    }

    pub fn set_owner(hwnd: isize, owner: isize) {
        // SAFETY: GWLP_HWNDPARENT on a top-level window sets its owner (the documented way to do
        // it after creation); both are windows of this thread.
        unsafe {
            #[cfg(target_pointer_width = "64")]
            SetWindowLongPtrW(hwnd as Hwnd, GWLP_HWNDPARENT, owner);
            #[cfg(not(target_pointer_width = "64"))]
            SetWindowLongW(hwnd as Hwnd, GWLP_HWNDPARENT, owner as i32);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_views_window_keeps_its_shape_from_any_corner() {
        let was = [100, 100, 980, 650];
        // Bottom right dragged mostly across: the height follows the width.
        assert_eq!(keep_ratio([100, 100, 1060, 660], was, 8, 1.6, [0, 0], 640), [100, 100, 1060, 700]);
        // Top left dragged mostly down: the width follows the height, the bottom right stays.
        assert_eq!(keep_ratio([90, 50, 980, 650], was, 4, 1.6, [0, 0], 640), [20, 50, 980, 650]);
        // Never under the least width; a frame round the inside is kept out of the shape.
        assert_eq!(keep_ratio([100, 100, 400, 300], was, 8, 1.6, [0, 0], 640), [100, 100, 740, 500]);
        assert_eq!(keep_ratio([100, 100, 1076, 700], [100, 100, 996, 682], 8, 1.6, [16, 32], 640), [100, 100, 1076, 732]);
        // A side (the keyboard's Size): the other side comes along.
        assert_eq!(keep_ratio([100, 100, 1140, 650], was, 2, 1.6, [0, 0], 640), [100, 100, 1140, 750]);
        assert_eq!(keep_ratio([100, 100, 980, 750], was, 6, 1.6, [0, 0], 640), [100, 100, 1140, 750]);
    }

    #[cfg(windows)]
    #[test]
    fn a_real_window_keeps_the_views_shape_while_it_is_sized() {
        // A hidden window 880 x 550 sized from its bottom right corner, as Windows sizes it in a drag.
        let hwnd = win32::test_window([100, 100, 980, 650]);
        assert_ne!(hwnd, 0, "no test window");
        let drag = [100, 100, 1300, 700];
        assert!(win32::keep_shape(hwnd, Some(1.6), 640));
        assert_eq!(win32::sizing(hwnd, 8, drag), [100, 100, 1300, 850]);
        // From the bottom only (the keyboard's Size): the width comes along.
        assert_eq!(win32::sizing(hwnd, 6, [100, 100, 980, 750]), [100, 100, 1140, 750]);
        // Let go: Windows' size stands.
        assert!(win32::keep_shape(hwnd, None, 0));
        assert_eq!(win32::sizing(hwnd, 8, drag), drag);
        win32::destroy(hwnd);
    }

    #[test]
    fn a_corner_drag_grows_smoothly() {
        // Windows proposes each size from the cursor and where the drag began: across a drag at 45
        // degrees or mostly sideways the window only grows, without jumping between two sizes.
        let was = [0, 0, 1100, 688];
        for (dx, dy) in [(10, 10), (10, 2), (2, 10)] {
            let mut last = 0;
            for k in 1..40 {
                let r = keep_ratio([0, 0, 1100 + dx * k, 688 + dy * k], was, 8, 1.6, [0, 0], 800);
                assert!(r[2] >= last, "{dx},{dy} step {k}: {r:?} after width {last}");
                assert!(((r[3] as f32) - (r[2] as f32) / 1.6).abs() <= 1.0, "{r:?}");
                last = r[2];
            }
        }
    }

    #[test]
    fn the_views_window_opens_at_her_size_or_as_she_left_it() {
        assert_eq!(stage_size_or_default(None, None), STAGE_SIZE);
        assert_eq!(stage_size_or_default(Some([1200.0, 9.0]), None), vec2(1200.0, 750.0));
        // Too small, damaged, or past the screen.
        assert_eq!(stage_size_or_default(Some([100.0, 100.0]), None), STAGE_MIN_SIZE);
        assert_eq!(stage_size_or_default(Some([f32::NAN, 1.0]), None), STAGE_SIZE);
        assert_eq!(stage_size_or_default(Some([3000.0, 1875.0]), Some(vec2(1536.0, 864.0))), vec2(1244.0, 778.0));
    }

    #[test]
    fn remembered_sizes_are_sanitized() {
        assert_eq!(full_size_or_default(None), FULL_DEFAULT_SIZE);
        assert_eq!(full_size_or_default(Some([1300.0, 900.0])), vec2(1300.0, 900.0));
        // Too small (e.g. 399 after rounding at a zoom): grown to the minimum; absurd: capped.
        assert_eq!(full_size_or_default(Some([100.0, 50.0])), FULL_MIN_SIZE);
        assert_eq!(full_size_or_default(Some([399.0, 900.0])), vec2(FULL_MIN_SIZE.x, 900.0));
        assert_eq!(full_size_or_default(Some([1e9, 900.0])), vec2(16_384.0, 900.0));
        // Damaged values fall back to the default.
        assert_eq!(full_size_or_default(Some([f32::NAN, 800.0])), FULL_DEFAULT_SIZE);
        assert_eq!(full_size_or_default(Some([800.0, f32::INFINITY])), FULL_DEFAULT_SIZE);
        assert_eq!(full_size_or_default(Some([0.0, 700.0])), FULL_DEFAULT_SIZE);
        assert_eq!(full_size_or_default(Some([-5.0, 700.0])), FULL_DEFAULT_SIZE);
    }

    #[test]
    fn finding_the_window_is_safe_without_one() {
        // A test thread owns no windows: the lookup must simply come back empty.
        let found = std::thread::spawn(main_window_handle).join().unwrap();
        assert_eq!(found, None);
        assert!(!point_on_a_monitor(-1.0e6, -1.0e6) || cfg!(not(windows)));
    }

    /// A close sent to winit's helper reaches the main window, and the helper stays. Two hidden
    /// "STATIC" windows stand in for them: like the helper, a STATIC window destroys itself when
    /// told to close.
    #[cfg(windows)]
    #[test]
    fn a_close_sent_to_the_helper_goes_to_the_main_window() {
        use std::ffi::c_void;

        type Hwnd = *mut c_void;
        /// MSG, filled in by Windows.
        #[repr(C)]
        #[derive(Default)]
        struct Msg {
            hwnd: isize,
            message: u32,
            wparam: usize,
            lparam: isize,
            time: u32,
            pt: [i32; 2],
            private: u32,
        }
        const WM_CLOSE: u32 = 0x0010;
        const WS_POPUP: u32 = 0x8000_0000;
        const PM_REMOVE: u32 = 1;

        #[link(name = "user32")]
        unsafe extern "system" {
            #[allow(clippy::too_many_arguments)]
            fn CreateWindowExW(
                ex_style: u32,
                class: *const u16,
                name: *const u16,
                style: u32,
                x: i32,
                y: i32,
                w: i32,
                h: i32,
                parent: Hwnd,
                menu: *mut c_void,
                instance: *mut c_void,
                param: *const c_void,
            ) -> Hwnd;
            fn DestroyWindow(hwnd: Hwnd) -> i32;
            fn IsWindow(hwnd: Hwnd) -> i32;
            fn SendMessageW(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize;
            fn PeekMessageW(msg: *mut Msg, hwnd: Hwnd, first: u32, last: u32, remove: u32) -> i32;
        }

        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        let null = std::ptr::null_mut();
        // SAFETY: plain Win32 calls on hidden windows this test makes on its own thread; the class
        // name outlives every call and `msg` is a valid out-struct.
        unsafe {
            let window = || CreateWindowExW(0, class.as_ptr(), std::ptr::null(), WS_POPUP, 0, 0, 0, 0, null, null, null, std::ptr::null());
            // Left alone, a window told to close is destroyed: what happened to winit's helper.
            let plain = window();
            assert!(!plain.is_null());
            SendMessageW(plain, WM_CLOSE, 0, 0);
            assert_eq!(IsWindow(plain), 0);

            let (helper, main) = (window(), window());
            assert!(win32::hook_helper(helper as isize, main as isize));
            SendMessageW(helper, WM_CLOSE, 0, 0);
            assert_ne!(IsWindow(helper), 0, "the helper stays");
            let mut msg = Msg::default();
            assert_ne!(PeekMessageW(&mut msg, main, WM_CLOSE, WM_CLOSE, PM_REMOVE), 0, "the main window was told to close");
            assert_eq!((msg.hwnd, msg.message), (main as isize, WM_CLOSE));
            DestroyWindow(helper);
            DestroyWindow(main);
        }
    }

    fn r(left: i32, top: i32, right: i32, bottom: i32) -> ScreenRect {
        ScreenRect { left, top, right, bottom }
    }

    const SCREEN: ScreenRect = ScreenRect { left: 0, top: 0, right: 1920, bottom: 1040 };

    #[test]
    fn settings_is_centred_over_the_main_window() {
        assert_eq!(centred_over(r(100, 100, 1100, 900), SCREEN, 560, 540), r(320, 230, 880, 770));
        // Near the screen's edge: moved onto it.
        assert_eq!(centred_over(r(-400, 0, 200, 600), SCREEN, 560, 540), r(0, 30, 560, 570));
        assert_eq!(centred_over(r(1700, 800, 2300, 1400), SCREEN, 560, 540), r(1360, 500, 1920, 1040));
        // Bigger than the screen: shrunk to it.
        assert_eq!(centred_over(r(0, 0, 600, 400), r(0, 0, 500, 400), 560, 540), r(0, 0, 500, 400));
    }

    #[test]
    fn placing_is_safe_without_a_window() {
        // Unknown handles (here: none) are simply not placed or measured.
        let found = std::thread::spawn(|| find_window("no such window 7c1e")).join().unwrap();
        assert_eq!(found, None);
        assert_eq!(visible_rect(0), None);
        assert!(!is_minimized(0));
    }

    #[test]
    fn positions_must_be_plausible() {
        assert_eq!(plausible_position(Some([-1920.0, 40.0])), Some([-1920.0, 40.0]));
        assert_eq!(plausible_position(Some([f32::INFINITY, 0.0])), None);
        assert_eq!(plausible_position(Some([1e7, 0.0])), None);
        assert!(moved(None, [1.0, 1.0]));
        assert!(!moved(Some([1.0, 1.0]), [1.2, 0.9]));
        assert!(moved(Some([1.0, 1.0]), [3.0, 1.0]));
    }
}
