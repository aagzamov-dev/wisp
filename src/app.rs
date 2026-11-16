//! Launcher lifecycle, input and message loop.

#[path = "start.rs"]
pub(crate) mod start;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant, SystemTime};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::DROPEFFECT_NONE;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::SHDoDragDrop;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, Result, w};

use crate::commands::{self, Command, Commands};
use crate::config::{Config, Position, Theme};
use crate::dock::space::DockSpace;
use crate::dock::{self, Dock};
use crate::drag::{self, Payload};
use crate::files::{FileResults, FileSearch};
use crate::history::History;
use crate::indexer::{AppEntry, IconPixels, IndexEvent, Indexer};
use crate::render::{Frame, Renderer, SurfaceFrame, layout};
use crate::search::{self, Action, Drill, FileScope, Kind, ResultItem, Searcher};
use crate::surface::nav::{self, Direction, Rect};
use crate::surface::{self, Input, Library, Widget};
use crate::taskbar::{self, TaskbarHider};
use crate::theme::Palette;
use crate::{hotkey, launch, system, tray, util};

const WM_APP_FRAME: u32 = WM_APP + 8;
const WM_APP_INDEX: u32 = WM_APP + 2;
const WM_APP_FILES: u32 = WM_APP + 5;
const WM_APP_SHOW: u32 = WM_APP + 3;
const WM_APP_QUIT: u32 = WM_APP + 4;
/// wisp-settings posts this message after it changes the config.
const WM_APP_RELOAD: u32 = WM_APP + 6;
/// Drives the scripted capture of the dock and the launcher.
const REEL_TIMER: usize = 96;
const REEL_MS: u32 = 50;
const WM_APP_COMMANDS: u32 = WM_APP + 7;
const CLASS_NAME: PCWSTR = crate::channel::LAUNCHER_CLASS;
const HOTKEY_ID: i32 = 1;
const CARET_TIMER: usize = 1;
/// Reads the state again while a surface is open, so it stays live.
const SURFACE_TIMER: usize = 2;
const SURFACE_REFRESH_MS: u32 = 1000;
/// Checks whether the user is done with a taskbar shown from the dock.
const PEEK_TIMER: usize = 3;
/// Tries again to hide the taskbar after a failure. At sign-in Explorer may
/// not have finished creating it.
const TASKBAR_TIMER: usize = 4;
const START_ACTIVATE_TIMER: usize = 5;
const TASKBAR_RETRY_MS: u32 = 2000;
const TASKBAR_ATTEMPTS: u32 = 5;
/// File results shown after apps, and for a query that starts with `?`.
const MIXED_FILE_RESULTS: usize = 5;
const ONLY_FILE_RESULTS: usize = 30;
const RESCAN_AFTER: Duration = Duration::from_secs(120);
const PLACEHOLDER: &str = "Search apps and files, or try wifi, vol, time";
/// Space below the launcher at the bottom of the screen, in DIPs. Matches the
/// dock's margin so the query box covers the dock.
const BOTTOM_MARGIN: f32 = 4.0;

static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

// Preserve index updates and hide requests during reentrant message handling.
const DEFER_INDEX: u8 = 1;
const DEFER_FILES: u8 = 4;
const DEFER_HIDE: u8 = 2;
const DEFER_COMMANDS: u8 = 8;
const DEFER_REMOTE: u8 = 16;
const DEFER_RELOAD: u8 = 32;
const DEFER_PAINT: u8 = 64;

thread_local! {
    static FRAME_PENDING: RefCell<Option<Arc<AtomicBool>>> = const {RefCell::new(None)};
    static LAUNCHER: RefCell<Option<Launcher>> = const { RefCell::new(None) };
    static DEFERRED: Cell<u8> = const { Cell::new(0) };
    static REMOTE_NOTIFICATIONS: RefCell<Option<crate::remote_session::Notifications>> = const { RefCell::new(None) };
    static REMOTE_SESSION_ID: Cell<Option<u32>> = const { Cell::new(None) };
    static DRAGGING: Cell<bool> = const { Cell::new(false) };
}

// Independent of Launcher borrows so nested native loops can always unregister
// before destroying the registered HWND.
fn unregister_remote_notifications() {
    REMOTE_NOTIFICATIONS.with(|slot| {
        slot.borrow_mut().take();
    });
}

/// Returns `None` if the launcher is already borrowed. Windows can send
/// reentrant messages during `ShowWindow` or a modal `ShellExecuteEx` call.
/// Runs deferred work after the outer handler releases the borrow.
fn with_launcher<R>(f: impl FnOnce(&mut Launcher) -> R) -> Option<R> {
    let result = LAUNCHER.with(|cell| cell.try_borrow_mut().ok().and_then(|mut l| l.as_mut().map(f)));
    if result.is_some() {
        run_deferred();
    }
    result
}

/// Defers `work` if a reentrant call cannot borrow the launcher.
fn with_launcher_or_defer(work: u8, f: impl FnOnce(&mut Launcher)) {
    if with_launcher(f).is_none() {
        DEFERRED.with(|d| d.set(d.get() | work));
    }
}

fn run_deferred() {
    loop {
        let work = DEFERRED.with(|d| d.replace(0));
        if work == 0 {
            return;
        }
        let ran = LAUNCHER.with(|cell| {
            let Ok(mut launcher) = cell.try_borrow_mut() else {
                return false;
            };
            if let Some(l) = launcher.as_mut() {
                if work & DEFER_REMOTE != 0 {
                    l.refresh_remote_session();
                }
                if work & DEFER_RELOAD != 0 {
                    l.reload_config_if_changed();
                }
                if work & DEFER_INDEX != 0 {
                    l.drain_index_events();
                }
                if work & DEFER_FILES != 0 {
                    l.drain_file_results();
                }
                if work & DEFER_COMMANDS != 0 {
                    l.drain_command_events();
                }
                if work & DEFER_HIDE != 0 {
                    l.hide_after_deactivation();
                }
                if work & DEFER_PAINT != 0 {
                    l.invalidate();
                }
            }
            true
        });
        if !ran {
            DEFERRED.with(|d| d.set(d.get() | work));
            return;
        }
    }
}

pub fn run() -> Result<()> {
    crate::logging::event("app.run.start", format_args!("ui_thread=true"));
    let _trace = util::perf::Session::new();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().any(|a| a == name);

    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ole = drag::Ole::new()?;

        // The mutex limits Wisp to one running instance.
        let _mutex = CreateMutexW(None, true, crate::channel::INSTANCE_MUTEX)?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // Wait up to 3 s for a starting instance to create its window.
            for _ in 0..30 {
                if let Ok(existing) = FindWindowW(CLASS_NAME, PCWSTR::null()) {
                    let mut pid = 0;
                    GetWindowThreadProcessId(existing, Some(&mut pid));
                    let _ = AllowSetForegroundWindow(pid);
                    let msg = if flag("--quit") { WM_APP_QUIT } else { WM_APP_SHOW };
                    let _ = PostMessageW(Some(existing), msg, WPARAM(0), LPARAM(0));
                    crate::logging::event("instance.forwarded", format_args!("quit={}", flag("--quit")));
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            return Err(windows::core::Error::new(
                E_FAIL,
                "Wisp is running, but its launcher window was not found within 3 s.",
            ));
        }
        if flag("--quit") {
            return Ok(());
        }

        let renderer = Renderer::new()?;
        let system_dpi = GetDpiForSystem();
        let instance = GetModuleHandleW(None)?;
        // build.rs embeds the icon as resource ID 1.
        let load_icon = |metric| -> Result<HICON> {
            let size = GetSystemMetricsForDpi(metric, system_dpi);
            let icon = LoadImageW(
                Some(instance.into()),
                PCWSTR(1 as _),
                IMAGE_ICON,
                size,
                size,
                LR_DEFAULTCOLOR,
            )?;
            Ok(HICON(icon.0))
        };
        let small_icon = load_icon(SM_CXSMICON)?;
        let large_icon = load_icon(SM_CXICON)?;

        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIcon: large_icon,
            hIconSm: small_icon,
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        if RegisterClassExW(&class) == 0 {
            return Err(windows::core::Error::from_thread());
        }
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);

        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            CLASS_NAME,
            &HSTRING::from(crate::channel::NAME),
            WS_POPUP | WS_CAPTION,
            0,
            0,
            100,
            100,
            None,
            None,
            Some(instance.into()),
            None,
        )?;

        // Apply WM_NCCALCSIZE before the first draw to remove the visible frame.
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )?;

        // Config::load creates the config on first run.
        let first_run = !Config::path().exists();
        let (config, config_error) = Config::load();
        crate::logging::event(
            "config.initial",
            format_args!(
                "valid={} first_run={first_run} topbar={} dock={}",
                config_error.is_none(),
                config.topbar,
                config.dock
            ),
        );
        let (events_tx, events_rx) = channel();
        let (files_tx, files_rx) = channel();
        // Icons are drawn at 32 DIPs; extract them at the system DPI's pixel size.
        let icon_size = (32 * system_dpi as i32 + 95) / 96;
        let (commands, command_events) = Commands::spawn(hwnd, WM_APP_COMMANDS, icon_size);
        let launcher = Launcher {
            hwnd,
            saved_config: config.clone(),
            config,
            remote_mode: crate::remote_mode::Mode::Auto,
            remote_rdp: false,
            config_mtime: Config::modified(),
            registered_hotkey: None,
            tray_icon: small_icon,
            palette: Palette::new(true, true),
            renderer,
            dpi: system_dpi,
            position: (0, 0),
            rows_that_fit: usize::MAX,
            visible: false,
            start_open: false,
            start_activating: false,
            start_focus: None,
            footer_menu: None,
            start_hover: None,
            footer_hover: None,
            start_height: start::HEIGHT,
            start_work: RECT::default(),
            start_width: start::WIDTH,
            start_motion: start::Motion::default(),
            start_motion_tick: None,
            start_alpha: crate::theme::WindowOpacity::default(),
            start_session: 0,
            app_generation: 0,
            start_menu_active: false,
            start_native_action: false,
            start_inline_press: None,
            start_previous_editor: None,
            pins: start::Pins::load(),
            win_key: None,
            editor: crate::editor::Editor::default(),
            searcher: Searcher::new(),
            results: Vec::new(),
            base_results: Vec::new(),
            file_results: Vec::new(),
            file_query: None,
            selected: 0,
            scroll: 0,
            caret_visible: true,
            last_mouse: None,
            drag_press: None,
            drag_consumed: false,
            apps: Vec::new(),
            icons: HashMap::new(),
            history: History::load(),
            indexer: Indexer::spawn(hwnd, WM_APP_INDEX, events_tx, icon_size),
            index_events: events_rx,
            file_search: FileSearch::spawn(hwnd, WM_APP_FILES, files_tx, icon_size),
            file_events: files_rx,
            last_scan: Instant::now(),
            commands,
            command_events,
            command_state: commands::State::default(),
            list: None,
            armed: None,
            scope: None,
            surfaces: Library::load(),
            surface_errors: Vec::new(),
            surface: None,
            commands_sent: 0,
            taskbar: None,
            taskbar_failures: 0,
            dock_space: None,
            dock: None,
            notch: None,
            morph: None,
            morphed: false,
            outline: None,
            previous_foreground: HWND::default(),
        };
        LAUNCHER.with(|cell| *cell.borrow_mut() = Some(launcher));
        with_launcher(|l| {
            l.start();
            if crate::util::perf::hook_on("WISP_CAPTURE") {
                if !matches!(
                    crate::util::perf::hook("WISP_CAPTURE_SCENARIO").as_deref(),
                    Some("hidden" | "reel")
                ) && let Some(dock) = &mut l.dock
                {
                    dock.hold(
                        None,
                        crate::util::perf::hook("WISP_CAPTURE_DOCK_PART").as_deref(),
                        crate::util::perf::hook("DOCK_DEMO_DPI").and_then(|s| s.parse().ok()),
                    );
                }
                if let Some(query) = crate::util::perf::hook("WISP_CAPTURE_QUERY") {
                    l.editor.insert(&query);
                }
                SetTimer(
                    Some(l.hwnd),
                    99,
                    crate::util::perf::hook("WISP_CAPTURE_DURATION_MS")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(3500),
                    None,
                );
                #[cfg(feature = "perf-trace")]
                if let Some(scenario) = crate::util::perf::hook("WISP_PERF_SCENARIO") {
                    if scenario.ends_with("hidden")
                        && let Some(dock) = &mut l.dock
                    {
                        dock.hide();
                        dock.show();
                    }
                    if !scenario.starts_with("idle") {
                        SetTimer(Some(l.hwnd), 97, 250, None);
                    }
                }
                if crate::util::perf::hook("WISP_CAPTURE_SCENARIO").as_deref() == Some("reel") {
                    // The app icons load during the first 4 s.
                    SetTimer(Some(l.hwnd), REEL_TIMER, 4000, None);
                } else if crate::util::perf::hook_on("WISP_CAPTURE_SCENARIO") {
                    SetTimer(Some(l.hwnd), 98, 45, None);
                }
            }
            if let Some(error) = config_error {
                tray::notify(l.hwnd, "Wisp config", &error, true);
            }
            if flag("--show") {
                l.show();
            } else if first_run {
                // Onboarding is optional. Without the settings app, Wisp starts as usual.
                let _ = launch::settings(true);
            }
        });

        util::perf::event("ready");
        crate::logging::event("app.ready", format_args!("message_loop=starting"));
        let active = Arc::new(AtomicBool::new(false));
        let pending = Arc::new(AtomicBool::new(false));
        FRAME_PENDING.with(|slot| *slot.borrow_mut() = Some(pending.clone()));
        let alive = Arc::new(AtomicBool::new(true));
        let frame_active = active.clone();
        let frame_pending = pending.clone();
        let frame_alive = alive.clone();
        let window = hwnd.0 as usize;
        let pacing = std::thread::spawn(move || {
            while frame_alive.load(Ordering::Acquire) {
                if !frame_active.load(Ordering::Acquire) {
                    std::thread::park();
                    continue;
                }
                let _ = windows::Win32::Graphics::Dwm::DwmFlush();
                if !frame_pending.swap(true, Ordering::AcqRel) {
                    let _ = PostMessageW(Some(HWND(window as _)), WM_APP_FRAME, WPARAM(0), LPARAM(0));
                }
            }
        });
        let mut msg = MSG::default();
        loop {
            let animating =
                with_launcher(|l| l.morph.is_some() || l.dock.as_ref().is_some_and(Dock::animating)).unwrap_or(false);
            active.store(animating, Ordering::Release);
            if animating {
                pacing.thread().unpark();
            }
            let received = GetMessageW(&mut msg, None, 0, 0).0;
            if received <= 0 {
                if received < 0 {
                    crate::logging::error(
                        "ui.message_loop",
                        format_args!("code={:#010X}", windows::core::Error::from_thread().code().0),
                    );
                }
                crate::logging::event(
                    "ui.message_loop.end",
                    format_args!("quit={} exit_code={}", received == 0, msg.wParam.0),
                );
                break;
            }
            dispatch_ui_message(&msg);
        }
        alive.store(false, Ordering::Release);
        pacing.thread().unpark();
        let _ = pacing.join();
        FRAME_PENDING.with(|slot| slot.borrow_mut().take());
        LAUNCHER.with(|cell| cell.borrow_mut().take());
        crate::logging::event("app.resources.released", format_args!("launcher=true"));
        if let Err(error) = dock::topbar_blur::shutdown_current_thread() {
            eprintln!("Could not complete island glass shutdown: {error}");
        }
    }
    Ok(())
}

struct StartAppMenu {
    generation: u64,
    owner: HWND,
    session: u64,
    app: AppEntry,
    anchor: POINT,
    actions: Vec<start::AppMenuAction>,
    dock_pins: Option<dock::AppPins>,
}

struct StartAppEffect {
    owner: HWND,
    session: u64,
    app: AppEntry,
    action: start::AppMenuAction,
    dock_pins: Option<dock::AppPins>,
}

struct Launcher {
    hwnd: HWND,
    config: Config,
    saved_config: Config,
    remote_mode: crate::remote_mode::Mode,
    remote_rdp: bool,
    config_mtime: Option<SystemTime>,
    /// Registered shortcut; may differ from `config.hotkey` if registration fails.
    registered_hotkey: Option<String>,
    tray_icon: HICON,
    palette: Palette,
    renderer: Renderer,
    dpi: u32,
    /// Screen coordinates of the launcher's left edge and its top edge, or its
    /// bottom edge at the bottom of the screen, in physical pixels.
    position: (i32, i32),
    /// Number of results that fit in the current monitor's work area.
    rows_that_fit: usize,
    visible: bool,
    start_open: bool,
    start_activating: bool,
    start_focus: Option<usize>,
    footer_menu: Option<(start::FooterMenu, usize)>,
    start_hover: Option<usize>,
    footer_hover: Option<usize>,
    start_height: f32,
    start_work: RECT,
    start_width: f32,
    start_motion: start::Motion,
    start_motion_tick: Option<Instant>,
    start_alpha: crate::theme::WindowOpacity,
    start_session: u64,
    app_generation: u64,
    start_menu_active: bool,
    start_native_action: bool,
    start_inline_press: Option<(AppEntry, start::AppMenuAction, u64)>,
    start_previous_editor: Option<crate::editor::Editor>,
    pins: start::Pins,
    win_key: Option<start::WinKey>,

    editor: crate::editor::Editor,
    searcher: Searcher,
    /// What the launcher shows: base results followed by file results.
    results: Vec<ResultItem>,
    /// Results computed on the UI thread: apps, calculator, URL, and command.
    base_results: Vec<ResultItem>,
    /// File search results, which arrive later from the file search worker.
    file_results: Vec<ResultItem>,
    /// The file query sent to the worker, if the current query searches files.
    file_query: Option<String>,
    selected: usize,
    scroll: usize,
    caret_visible: bool,
    /// Last cursor position in screen pixels, independent of the launcher layout.
    last_mouse: Option<(i32, i32)>,
    drag_press: Option<((i32, i32), Payload)>,
    drag_consumed: bool,

    apps: Vec<AppEntry>,
    icons: HashMap<String, Arc<IconPixels>>,
    history: History,
    indexer: Indexer,
    index_events: Receiver<IndexEvent>,
    file_search: FileSearch,
    file_events: Receiver<FileResults>,
    last_scan: Instant,

    commands: Commands,
    command_events: Receiver<commands::Event>,
    command_state: commands::State,
    /// The items of the open list, such as Wi-Fi networks. `None` while loading.
    list: Option<Vec<ResultItem>>,
    /// A command waiting for a second Enter, such as Restart.
    armed: Option<Command>,
    /// The list Tab opened, if any.
    scope: Option<Scope>,

    surfaces: Library,
    /// Problems with the user's surface files that the user has been told about.
    surface_errors: Vec<String>,
    surface: Option<OpenSurface>,
    /// Commands sent to the worker. State read before the worker ran all of
    /// them would undo changes the launcher already shows.
    commands_sent: u64,

    /// Hides the Windows taskbar while `hide_taskbar` is on.
    taskbar: Option<TaskbarHider>,
    /// Failed attempts to hide or restore the taskbar since the config changed.
    taskbar_failures: u32,
    /// Room kept for a dock that stays visible.
    dock_space: Option<DockSpace>,
    dock: Option<Dock>,
    notch: Option<crate::notch::Notch>,
    morph: Option<Morph>,
    morphed: bool,
    outline: Option<RECT>,
    previous_foreground: HWND,
}

struct Morph {
    from: RECT,
    to: RECT,
    start: Instant,
    opening: bool,
    from_opacity: f32,
}

/// A surface shown instead of the results. The query filters its widgets.
struct OpenSurface {
    /// Index into `Launcher::surfaces`.
    index: usize,
    widgets: Vec<Widget>,
    rects: Vec<Rect>,
    focus: usize,
    /// The cursor inside the focused widget, such as an output device.
    stop: usize,
    scroll: f32,
    /// The query before the surface opened. Leaving it restores the query.
    query: String,
}

/// A list that Tab opens on a result: its actions, or a command's items such
/// as Wi-Fi networks. The query filters the list.
struct Scope {
    /// Shown before the query.
    title: String,
    drill: Option<Drill>,
    /// The result's actions, when `drill` is `None`.
    actions: Vec<ResultItem>,
    /// The query before Tab. Leaving the list restores it.
    query: String,
}

impl Launcher {
    fn start(&mut self) {
        let notifications = crate::remote_session::Notifications::new(self.hwnd);
        REMOTE_SESSION_ID.set(notifications.session);
        REMOTE_NOTIFICATIONS.with(|slot| *slot.borrow_mut() = Some(notifications));
        self.read_remote_mode();
        self.remote_rdp = self.query_remote_session();
        self.config = self.saved_config.for_remote(self.remote_active());
        self.sync_shell();
        self.apply_theme();
        self.set_collapsed_style(true);
        if self.unified() {
            self.paint();
        }
        // Windows needs the tray icon before it can show notifications.
        tray::add(self.hwnd, self.tray_icon, &self.tooltip());
        if !crate::util::perf::hook_on("WISP_CAPTURE") {
            self.register_hotkey(true);
        }
        self.sync_win_key();
        self.indexer.rescan();
        self.last_scan = Instant::now();
    }

    fn sync_win_key(&mut self) {
        if !self.config.win_start || crate::util::perf::hook_on("WISP_CAPTURE") {
            self.win_key = None;
        } else if self.win_key.is_none() {
            match start::WinKey::install(self.hwnd) {
                Ok(hook) => self.win_key = Some(hook),
                Err(error) => tray::notify(self.hwnd, "Windows key", &error.message(), true),
            }
        }
    }

    fn tooltip(&self) -> String {
        match &self.registered_hotkey {
            Some(hotkey) => format!("{} ({hotkey})", crate::channel::NAME),
            None => format!("{} (no shortcut)", crate::channel::NAME),
        }
    }

    /// Tries to restore the previous shortcut if the new shortcut cannot be
    /// registered, so an invalid config does not disable access to the launcher.
    fn register_hotkey(&mut self, report_errors: bool) {
        if self.registered_hotkey.as_deref() == Some(self.config.hotkey.as_str()) {
            return;
        }
        let wanted = match hotkey::parse(&self.config.hotkey) {
            Ok(hotkey) => hotkey,
            Err(error) => {
                if report_errors {
                    tray::notify(self.hwnd, "Wisp config", &error, true);
                }
                return;
            }
        };
        let hwnd = self.hwnd;
        let register =
            |hk: &hotkey::Hotkey| unsafe { RegisterHotKey(Some(hwnd), HOTKEY_ID, hk.modifiers, hk.vk).is_ok() };

        let previous = self.registered_hotkey.take();
        if previous.is_some() {
            unsafe {
                let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
            }
        }
        if register(&wanted) {
            self.registered_hotkey = Some(self.config.hotkey.clone());
        } else {
            if let Some(previous) = previous
                && hotkey::parse(&previous).is_ok_and(|hk| register(&hk))
            {
                self.registered_hotkey = Some(previous);
            }
            if report_errors {
                let message = format!(
                    "Windows could not register {}. Another app may use it. Change \"hotkey\" in the config.",
                    self.config.hotkey
                );
                tray::notify(hwnd, "Could not register shortcut", &message, true);
            }
        }
        tray::set_tip(hwnd, &self.tooltip());
    }

    fn remote_active(&self) -> bool {
        self.remote_mode.active(self.remote_rdp)
    }

    fn read_remote_mode(&mut self) {
        match crate::remote_mode::load(&util::data_dir().join("remote-mode.txt")) {
            Ok(mode) => self.remote_mode = mode,
            Err(error) => crate::logging::error("remote.load", format_args!("error={error}")),
        }
    }

    fn query_remote_session(&self) -> bool {
        let result = crate::remote_session::protocol();
        if let Err(error) = &result {
            crate::logging::error("remote.query", format_args!("error={error}"));
        }
        crate::remote_session::observed(self.remote_rdp, result)
    }

    fn refresh_remote_session(&mut self) {
        let previous = self.remote_active();
        self.remote_rdp = self.query_remote_session();
        self.apply_remote_change(previous);
    }

    fn apply_remote_change(&mut self, previous: bool) {
        if previous != self.remote_active() {
            self.config = self.saved_config.for_remote(self.remote_active());
            crate::logging::event(
                "remote.layout.changed",
                format_args!(
                    "mode={} detected_rdp={} active={} dock_autohide={} dock_smart_hide={} topbar={}",
                    self.remote_mode.name(),
                    self.remote_rdp,
                    self.remote_active(),
                    self.config.dock_autohide,
                    self.config.dock_smart_hide,
                    self.config.topbar
                ),
            );
            self.taskbar_failures = 0;
            self.sync_shell();
            self.apply_theme();
            if self.visible {
                self.apply_size();
                self.paint();
            }
        }
    }

    fn reload_config_if_changed(&mut self) {
        let _span = util::perf::Span::new("config");
        let previous = self.remote_active();
        self.read_remote_mode();
        if self.remote_mode == crate::remote_mode::Mode::Auto {
            self.remote_rdp = self.query_remote_session();
        }
        self.apply_remote_change(previous);
        let mtime = Config::modified();
        if mtime == self.config_mtime {
            return;
        }
        self.config_mtime = mtime;
        crate::logging::event("config.reload", format_args!("stage=read"));
        let (config, error) = Config::load();
        if let Some(error) = error {
            // Keep the last valid config so a read or parse error does not change it.
            tray::notify(self.hwnd, "Wisp config", &error, true);
            return;
        }
        self.saved_config = config;
        self.config = self.saved_config.for_remote(self.remote_active());
        crate::logging::event(
            "config.applied",
            format_args!(
                "topbar={} dock={} island_blur={}",
                self.config.topbar, self.config.dock, self.config.topbar_island_blur
            ),
        );
        self.taskbar_failures = 0;
        self.sync_shell();
        self.apply_theme();
        self.register_hotkey(true);
        self.sync_win_key();
    }

    /// Hides or restores the Windows taskbar and creates or removes the dock
    /// to match the config.
    fn sync_shell(&mut self) {
        if let Some(notch) = &mut self.notch {
            notch.set_media_exclusions(&self.config.music_excluded_apps);
        }
        self.sync_taskbar();
        let shared = self.bottom() && crate::util::perf::hook_on("WISP_CAPTURE");
        if self.dock.as_ref().is_some_and(|d| d.shared() != shared) {
            self.dock = None;
        }
        if !self.config.dock && !self.config.topbar {
            self.dock = None;
            self.sync_music_notch();
            self.sync_reserve();
            return;
        }
        if self.dock.is_none() {
            match if shared {
                Dock::on_surface(self.hwnd)
            } else {
                Dock::new(self.hwnd)
            } {
                Ok(dock) => self.dock = Some(dock),
                Err(e) => {
                    tray::notify(self.hwnd, "Could not show the dock", &e.message(), true);
                    return;
                }
            }
        }
        let dark = self.dark_theme();
        if let Some(dock) = &mut self.dock {
            if let Err(error) = dock.sync_monitors() {
                eprintln!("Could not synchronize monitor docks: {error}");
            }
            dock.set_remote(self.remote_mode.active(self.remote_rdp));
            dock.apply_theme(
                dark,
                self.config.backdrop,
                &self.config.backdrop_color,
                self.config.material_options(),
            );
            dock.pace_frames();
            dock.set_apps(&self.apps, &self.icons);
            dock.set_window_list_mode(self.config.window_list);
            dock.set_app_icon_size(self.config.dock_icon_size as u32);
            dock.set_animation(
                self.config.dock_open_ms,
                self.config.dock_close_ms,
                self.config.dock_animation,
            );
            // Notification icons need a home while the taskbar is hidden.
            dock.set_tray(self.config.topbar || self.taskbar.as_ref().is_some_and(TaskbarHider::is_active));
            if let Err(error) = dock.set_topbar(&self.config) {
                eprintln!("Could not show top bar: {error}");
            }
            dock.set_media_exclusions(&self.config.music_excluded_apps);
            dock.set_visibility(self.config.dock_autohide, self.config.dock_smart_hide);
            dock.set_reveal_height(self.config.dock_reveal_height);
            dock.set_current_monitor_only(self.config.dock_current_monitor_only);
            dock.set_preview_current_monitor_only(self.config.dock_preview_current_monitor_only);
            if !self.config.dock {
                dock.hide();
            } else if self.start_open {
                dock.set_start_open(true);
            } else if self.visible {
                dock.hide();
            } else {
                dock.show();
            }
        }
        self.sync_music_notch();
        self.sync_reserve();
    }

    fn sync_music_notch(&mut self) {
        if !self.config.music_notch || self.dock.as_ref().is_some_and(Dock::topbar_active) {
            self.notch = None;
        } else if self.notch.is_none() {
            match crate::notch::Notch::new(self.hwnd) {
                Ok(notch) => self.notch = Some(notch),
                Err(error) => tray::notify(self.hwnd, "Could not show the music notch", &error.message(), true),
            }
        }
        let dark = self.dark_theme();
        if let Some(notch) = &self.notch {
            notch.apply_theme(
                dark,
                self.config.backdrop,
                &self.config.backdrop_color,
                self.config.material_options(),
            );
        }
    }

    /// Keeps maximized windows above a dock that stays visible while the
    /// taskbar is hidden. A shown taskbar keeps its own space.
    fn sync_reserve(&mut self) {
        let hidden = self.taskbar.as_ref().is_some_and(TaskbarHider::is_active);
        let wanted = self.config.dock && hidden && self.dock.as_ref().is_some_and(Dock::stays_visible);
        let reserved_height = self
            .dock
            .as_ref()
            .map_or(dock::DEFAULT_RESERVED_HEIGHT, Dock::reserved_height);
        // Before the space exists, so a re-hide while Windows places it
        // cannot take the bottom back.
        if wanted && let Some(hider) = &mut self.taskbar {
            hider.keep_bottom(true);
        }
        if !wanted {
            self.dock_space = None;
        } else if let Some(space) = &self.dock_space {
            // The taskbar hider may have just changed the work area.
            space.set_height(reserved_height);
            space.check();
        } else {
            match DockSpace::new(reserved_height) {
                Ok(space) => self.dock_space = Some(space),
                Err(e) => eprintln!("could not keep space for the dock: {e}"),
            }
        }
        if let Some(hider) = &mut self.taskbar {
            hider.keep_bottom(self.dock_space.is_some());
        }
    }

    /// Shows the Windows taskbar for a moment, from the dock's chevron menu.
    fn peek_taskbar(&mut self) {
        let Some(hider) = &mut self.taskbar else { return };
        if let Err(e) = hider.peek() {
            eprintln!("could not show the taskbar: {e}");
            return;
        }
        // The dock would cover the taskbar.
        if let Some(dock) = &mut self.dock {
            dock.hide();
        }
        unsafe {
            SetTimer(Some(self.hwnd), PEEK_TIMER, 250, None);
        }
    }

    fn peek_tick(&mut self) {
        let done = match &mut self.taskbar {
            Some(hider) => hider.peek_tick().unwrap_or_else(|e| {
                eprintln!("could not hide the taskbar: {e}");
                true
            }),
            None => true,
        };
        if done {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), PEEK_TIMER);
            }
            if let Some(dock) = &mut self.dock
                && !self.visible
            {
                dock.show();
            }
        }
    }

    /// Hides or restores the taskbar to match the config, or hides it again.
    /// Only the taskbar and the dock's notification area change here, so a
    /// retry cannot touch the launcher or the dock's visibility.
    fn sync_taskbar(&mut self) {
        let result = match &mut self.taskbar {
            Some(hider) if self.config.hide_taskbar => hider.reapply(),
            _ => TaskbarHider::configure(&mut self.taskbar, self.hwnd, self.config.hide_taskbar),
        };
        match result {
            Ok(()) => self.taskbar_failures = 0,
            Err(e) => self.taskbar_failed(&e),
        }
        let active = self.taskbar.as_ref().is_some_and(TaskbarHider::is_active);
        if let Some(dock) = &mut self.dock {
            // Notification icons need a home while the taskbar is hidden.
            dock.set_tray(active || self.config.topbar);
        }
    }

    /// Retries quietly every 2 seconds. Only the last failure is reported, by
    /// which time the tray icon exists to show the notification.
    fn taskbar_failed(&mut self, error: &windows::core::Error) {
        eprintln!("could not change the taskbar: {error}");
        self.taskbar_failures += 1;
        if self.taskbar_failures < TASKBAR_ATTEMPTS {
            unsafe {
                SetTimer(Some(self.hwnd), TASKBAR_TIMER, TASKBAR_RETRY_MS, None);
            }
        } else if self.taskbar_failures == TASKBAR_ATTEMPTS {
            tray::notify(self.hwnd, "Could not change the taskbar", &error.message(), true);
        }
    }

    /// Hides the taskbar again after Explorer or Windows shows it.
    fn on_taskbar_message(&mut self, msg: u32) {
        if let Some(hider) = &mut self.taskbar {
            match hider.on_message(msg) {
                // A later failure gets its retries again, but only once the
                // taskbar is really hidden: a failed restore also lands here.
                Ok(true) if self.config.hide_taskbar && hider.is_active() => self.taskbar_failures = 0,
                Ok(_) => {}
                Err(e) => self.taskbar_failed(&e),
            }
        }
        // Explorer restarting or the hider reclaiming space can take the
        // dock's space away.
        self.sync_reserve();
        // Re-hide and reclaim space first, since those operations can change
        // Explorer's z-order. Don't wait for the tray host's periodic check.
        if msg == taskbar::WM_TASKBAR_SHOW
            && let Some(dock) = &self.dock
        {
            dock.outrank_tray();
        }
    }

    fn update_dock_apps(&mut self) {
        if let Some(dock) = &mut self.dock {
            dock.set_apps(&self.apps, &self.icons);
        }
    }

    fn dark_theme(&self) -> bool {
        match self.config.theme {
            Theme::Dark => true,
            Theme::Light => false,
            Theme::System => !system::system_uses_light_theme(),
        }
    }

    fn apply_launcher_theme(&mut self, dark: bool) {
        self.palette = crate::theme::apply_backdrop(
            self.hwnd,
            dark,
            self.config.backdrop,
            self.start_open || self.unified(),
            &self.config.backdrop_color,
            self.config.material_options(),
        );
    }

    fn apply_theme(&mut self) {
        let dark = self.dark_theme();
        self.apply_launcher_theme(dark);
        if let Some(notch) = &self.notch {
            notch.apply_theme(
                dark,
                self.config.backdrop,
                &self.config.backdrop_color,
                self.config.material_options(),
            );
        }
        if let Some(dock) = &mut self.dock {
            dock.apply_theme(
                dark,
                self.config.backdrop,
                &self.config.backdrop_color,
                self.config.material_options(),
            );
        }
        self.invalidate();
    }

    fn toggle(&mut self) {
        if self.start_motion.closing() {
            self.hide_now();
            self.show();
            return;
        }
        if self.visible { self.hide() } else { self.show() }
    }

    fn toggle_start(&mut self) {
        if self.start_motion.closing() {
            self.reopen_start();
            return;
        }
        if self.visible {
            let was_start = self.start_open;
            self.hide();
            if was_start {
                return;
            }
        }
        self.start_open = true;
        self.start_session = self.start_session.wrapping_add(1);
        self.start_focus = None;
        self.start_previous_editor = Some(std::mem::take(&mut self.editor));
        self.show();
    }

    fn start_home(&self) -> bool {
        self.start_open && self.editor.text().trim().is_empty() && self.scope.is_none() && self.surface.is_none()
    }

    fn show(&mut self) {
        if self.start_motion.closing() {
            self.reopen_start();
            return;
        }
        let _span = util::perf::Span::new("show");
        util::perf::input("show_to_content");
        self.reload_config_if_changed();
        if !crate::util::perf::hook_on("WISP_CAPTURE") {
            self.register_hotkey(false);
        }
        self.commands.refresh();
        self.armed = None;
        self.leave_scope();
        self.leave_surface();
        if !self.start_open {
            self.load_surfaces();
        }
        if self.last_scan.elapsed() > RESCAN_AFTER {
            self.indexer.rescan();
            self.last_scan = Instant::now();
        }
        // Selecting the previous query lets the user replace it by typing.
        let should_morph =
            self.unified() && (self.morph.is_some() || self.dock.as_ref().is_some_and(Dock::mostly_visible));
        self.morphed = should_morph;
        let from = if self.unified() {
            let rect = self.outline.unwrap_or_else(|| self.current_rect());
            let suspended = self.dock.as_mut().unwrap().suspend();
            should_morph.then_some(if self.morph.is_some() || self.visible {
                rect
            } else {
                suspended
            })
        } else {
            None
        };
        let opacity = self.morph_opacity();
        if self.morph.is_none() {
            self.outline = None;
        }
        self.morph = from.map(|rect| Morph {
            from: rect,
            to: rect,
            start: Instant::now(),
            opening: true,
            from_opacity: opacity,
        });
        if !self.visible {
            self.previous_foreground = unsafe { GetForegroundWindow() };
        }
        self.editor.select_all();
        if self.start_open {
            self.start_motion = start::Motion::default();
            self.begin_start_motion(true);
            self.apply_launcher_theme(self.dark_theme());
            if let Some(dock) = &mut self.dock {
                dock.set_start_open(true);
            }
            self.place_on_active_monitor();
        }
        self.update_results();
        if !self.start_open {
            self.place_on_active_monitor();
        }
        if let Some(from) = from {
            self.begin_morph(from, self.launcher_rect(), true, opacity);
        }
        self.last_mouse = cursor_pos();
        self.visible = true;
        if !self.start_open
            && let Some(dock) = &mut self.dock
            && !dock.shared()
        {
            dock.hide();
        }
        self.set_collapsed_style(false);
        if self.start_open {
            self.start_activating = unsafe { SetTimer(Some(self.hwnd), START_ACTIVATE_TIMER, 70, None) } != 0;
            // Supply the first Start frame before ShowWindow can display a
            // previous launcher backbuffer while its paint callback reenters.
            self.apply_start_position(0.);
            self.paint();
        }
        unsafe {
            let _ = ShowWindow(
                self.hwnd,
                if crate::util::perf::hook_on("WISP_CAPTURE") {
                    SW_SHOWNOACTIVATE
                } else {
                    SW_SHOW
                },
            );
            if !crate::util::perf::hook_on("WISP_CAPTURE") {
                let _ = SetForegroundWindow(self.hwnd);
                let _ = SetFocus(Some(self.hwnd));
            }
            // DWM acrylic uses non-client activation as well as foreground state.
            if GetForegroundWindow() == self.hwnd {
                SendMessageW(self.hwnd, WM_NCACTIVATE, Some(WPARAM(1)), Some(LPARAM(-1)));
            }
        }
        self.restart_caret();
        self.paint();
    }

    fn hide_after_deactivation(&mut self) {
        let foreground = unsafe { GetForegroundWindow() };
        if self.start_menu_active
            || self.start_native_action
            || foreground == self.hwnd
            || self.start_activating && foreground == self.previous_foreground
        {
            return;
        }
        self.hide();
    }

    fn begin_start_motion(&mut self, opening: bool) {
        let animate = self.config.dock_open_ms != 0
            && (opening || self.config.dock_close_ms != 0)
            && dock::client_animations_enabled();
        self.start_motion
            .retarget_profile(opening, animate, self.remote_active());
        self.start_motion_tick = self.start_motion.active().then(Instant::now);
        unsafe {
            if self.start_motion.active() {
                if SetTimer(
                    Some(self.hwnd),
                    dock::POPUP_FADE_TIMER,
                    crate::animation::interval(self.remote_active()),
                    None,
                ) == 0
                {
                    self.start_motion.settle();
                    self.start_motion_tick = None;
                }
            } else {
                let _ = KillTimer(Some(self.hwnd), dock::POPUP_FADE_TIMER);
            }
        }
    }

    fn reopen_start(&mut self) {
        self.visible = true;
        self.start_activating = false;
        self.begin_start_motion(true);
        self.apply_start_position(0.);
        self.restart_caret();
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOW);
            let _ = SetForegroundWindow(self.hwnd);
            let _ = SetFocus(Some(self.hwnd));
        }
    }

    fn apply_start_position(&mut self, elapsed: f32) -> crate::animation::Phase {
        if !self.start_open {
            return self.start_motion.phase();
        }
        let mut presentation_failed = false;
        let phase = self.start_motion.advance_present(elapsed, |opacity| {
            if let Err(error) = self.start_alpha.present(self.hwnd, opacity) {
                eprintln!("Start opacity: {error}");
                presentation_failed = true;
                return false;
            }
            true
        });
        if phase == crate::animation::Phase::Hidden {
            // hide_now retires the HWND before clearing its opacity style.
            return phase;
        }
        if presentation_failed {
            self.start_alpha.clear(self.hwnd);
        }
        let rect = self.launcher_rect();
        let top = rect.top + self.px(self.start_motion.offset_dip());
        unsafe {
            let mut actual = RECT::default();
            if GetWindowRect(self.hwnd, &mut actual).is_ok() && actual.left == rect.left && actual.top == top {
                return phase;
            }
            let _ = SetWindowPos(
                self.hwnd,
                None,
                rect.left,
                top,
                0,
                0,
                SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER,
            );
        }
        phase
    }

    fn tick_start_motion(&mut self) {
        let Some(previous) = self.start_motion_tick else {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), dock::POPUP_FADE_TIMER);
            }
            return;
        };
        self.start_motion_tick = Some(Instant::now());
        let phase = self.apply_start_position(previous.elapsed().as_secs_f32());
        if !matches!(
            phase,
            crate::animation::Phase::Opening | crate::animation::Phase::Closing
        ) {
            self.start_motion_tick = None;
            unsafe {
                let _ = KillTimer(Some(self.hwnd), dock::POPUP_FADE_TIMER);
            }
            if phase == crate::animation::Phase::Hidden {
                self.hide_now();
            }
        }
        // The prepared window surface is retained. Opacity and translation do
        // not change its contents; actual content events invalidate separately.
    }

    fn finish_start_activation(&mut self) {
        if !self.start_activating {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), START_ACTIVATE_TIMER);
            }
            return;
        }
        self.start_activating = false;
        unsafe {
            let _ = KillTimer(Some(self.hwnd), START_ACTIVATE_TIMER);
            if self.start_open && GetForegroundWindow() == self.previous_foreground {
                let _ = SetForegroundWindow(self.hwnd);
                if GetForegroundWindow() == self.hwnd {
                    let _ = SetFocus(Some(self.hwnd));
                }
            }
        }
    }

    fn hide(&mut self) {
        self.drag_press = None;
        if self.start_motion.closing() {
            return;
        }
        if self.start_open && self.visible {
            self.visible = false;
            self.start_activating = false;
            self.footer_menu = None;
            self.footer_hover = None;
            self.start_hover = None;
            unsafe {
                let _ = KillTimer(Some(self.hwnd), START_ACTIVATE_TIMER);
                let _ = KillTimer(Some(self.hwnd), CARET_TIMER);
                let _ = KillTimer(Some(self.hwnd), SURFACE_TIMER);
            }
            self.begin_start_motion(false);
            if self.start_motion.active() {
                self.invalidate();
                return;
            }
        }
        self.hide_now();
    }

    fn hide_now(&mut self) {
        self.drag_press = None;
        if !self.visible && !self.start_open {
            return;
        }
        self.start_motion_tick = None;
        self.start_motion.retarget(false, false);
        unsafe {
            let _ = KillTimer(Some(self.hwnd), dock::POPUP_FADE_TIMER);
        }
        let opacity = self.morph_opacity();
        self.visible = false;
        self.start_activating = false;
        self.footer_menu = None;
        self.footer_hover = None;
        self.start_hover = None;
        self.start_inline_press = None;
        let was_start = std::mem::take(&mut self.start_open);
        if was_start {
            self.start_session = self.start_session.wrapping_add(1);
            self.start_menu_active = false;
            if let Some(editor) = self.start_previous_editor.take() {
                self.editor = editor;
            }
            if let Some(dock) = &mut self.dock {
                dock.set_start_open(false);
            }
        }
        if self.morphed && self.unified() && !self.taskbar.as_ref().is_some_and(TaskbarHider::is_peeking) {
            let to = self.dock.as_ref().unwrap().resting_rect();
            self.begin_morph(self.outline.unwrap_or_else(|| self.current_rect()), to, false, opacity);
            self.set_collapsed_style(true);
            unsafe {
                if GetForegroundWindow() == self.hwnd && !self.previous_foreground.is_invalid() {
                    let _ = SetForegroundWindow(self.previous_foreground);
                }
                let _ = KillTimer(Some(self.hwnd), CARET_TIMER);
                let _ = KillTimer(Some(self.hwnd), SURFACE_TIMER);
            }
            return;
        }
        unsafe {
            let _ = KillTimer(Some(self.hwnd), CARET_TIMER);
            let _ = KillTimer(Some(self.hwnd), SURFACE_TIMER);
            let _ = KillTimer(Some(self.hwnd), START_ACTIVATE_TIMER);
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        self.start_alpha.clear(self.hwnd);
        self.morph = None;
        self.outline = None;
        if was_start {
            self.apply_launcher_theme(self.dark_theme());
        } else {
            self.apply_theme();
        }
        self.set_collapsed_style(true);
        if let Some(dock) = &mut self.dock
            && !self.taskbar.as_ref().is_some_and(TaskbarHider::is_peeking)
        {
            if !was_start {
                dock.show();
            }
        }
    }

    /// Uses the monitor under the cursor. Reduces the result count and moves
    /// the launcher up if needed to keep it within the work area.
    fn place_on_active_monitor(&mut self) {
        let work = unsafe {
            let mut cursor = POINT::default();
            let _ = GetCursorPos(&mut cursor);
            if self.start_open
                && let Some(dock) = &self.dock
            {
                let anchor = dock.start_rect();
                cursor = POINT {
                    x: (anchor.left + anchor.right) / 2,
                    y: (anchor.top + anchor.bottom) / 2,
                };
            }
            let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let _ = GetMonitorInfoW(monitor, &mut info);
            // Use the target monitor's DPI before moving the launcher there.
            let (mut dpi_x, mut dpi_y) = (96, 96);
            let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
            self.set_dpi(
                crate::util::perf::hook("DOCK_DEMO_DPI")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(dpi_x),
            );
            if self.unified() { info.rcMonitor } else { info.rcWork }
        };

        let margin = self.px(16.0);
        if self.start_open {
            let scale = 96. / self.dpi as f32;
            self.start_work = work;
            let placed = start::placement(
                work,
                self.dock.as_ref().map(|dock| dock.start_rect().top),
                self.dpi,
                self.config.start_position,
            );
            self.start_width = (placed.right - placed.left) as f32 * scale;
            self.start_height = (placed.bottom - placed.top) as f32 * scale;
            self.rows_that_fit = start::rows(self.start_height);
            self.position = (placed.left, placed.bottom);
            self.apply_size();
            return;
        }
        let width = self.px(layout::WIDTH);
        let left = work.left + (work.right - work.left - width) / 2;
        if self.bottom() {
            // The query box covers the dock's spot, and results grow upward.
            let bottom = work.bottom - self.px(if self.unified() { 9.0 } else { BOTTOM_MARGIN });
            let available = (bottom - margin - work.top) as f32 * 96.0 / self.dpi as f32;
            self.rows_that_fit = (((available - layout::height(1)) / layout::ROW_H).max(0.0) as usize + 1).max(1);
            self.position = (left, bottom);
        } else {
            let full_height = self.px(layout::height(self.config.max_results));
            let top = (work.top + (work.bottom - work.top) / 4)
                .min(work.bottom - margin - full_height)
                .max(work.top + margin);
            let available = (work.bottom - margin - top) as f32 * 96.0 / self.dpi as f32;
            self.rows_that_fit = (((available - layout::height(1)) / layout::ROW_H).max(0.0) as usize + 1).max(1);
            self.position = (left, top);
        }
        self.apply_size();
    }

    fn set_dpi(&mut self, dpi: u32) {
        self.dpi = dpi;
        self.renderer.set_dpi(dpi);
    }

    fn on_dpi_changed(&mut self, dpi: u32) {
        if dpi != self.dpi {
            self.set_dpi(dpi);
            self.outline = None;
            self.morph = None;
            self.apply_theme();
            if self.visible {
                self.place_on_active_monitor();
                self.invalidate();
            } else if self.start_open {
                self.apply_size();
                self.invalidate();
            } else if let Some(dock) = &mut self.dock {
                dock.refresh();
            }
        }
    }

    /// The query box sits at the bottom of the screen and results stack upward.
    fn bottom(&self) -> bool {
        !self.start_open && self.config.position == Position::Bottom
    }

    /// The launcher's height in DIPs.
    fn window_h(&self) -> f32 {
        if self.start_open {
            self.start_height
        } else {
            layout::height(self.window_rows())
        }
    }

    fn px(&self, dips: f32) -> i32 {
        (dips * self.dpi as f32 / 96.0).round() as i32
    }

    fn list_rows(&self) -> usize {
        self.config.max_results.min(self.rows_that_fit)
    }

    fn visible_rows(&self) -> usize {
        self.results.len().min(self.list_rows())
    }

    /// Surfaces reserve their widget budget; search uses only populated rows.
    fn window_rows(&self) -> usize {
        if self.surface.is_some() {
            self.list_rows()
        } else if self.start_open || self.unified() {
            self.visible_rows()
        } else if self.results.is_empty() {
            0
        } else {
            self.list_rows()
        }
    }

    fn apply_size(&mut self) {
        if self.start_open && self.start_work.right > self.start_work.left {
            let home = self.start_home();
            let height = if home {
                start::home_height(self.start_width)
            } else {
                start::search_height(self.results.len(), self.config.max_results, self.surface.is_some())
            };
            let placed = start::placement_height(
                self.start_work,
                self.dock.as_ref().map(|dock| dock.start_rect().top),
                self.dpi,
                self.config.start_position,
                height,
            );
            self.start_height = (placed.bottom - placed.top) as f32 * 96. / self.dpi as f32;
            // Compact Home must not reduce the following search/surface budget.
            let list_height = if home {
                let full = start::placement(
                    self.start_work,
                    self.dock.as_ref().map(|dock| dock.start_rect().top),
                    self.dpi,
                    self.config.start_position,
                );
                (full.bottom - full.top) as f32 * 96. / self.dpi as f32
            } else {
                self.start_height
            };
            self.rows_that_fit = start::rows(list_height);
            self.position = (placed.left, placed.bottom);
        }
        let target = self.launcher_rect();
        if self.start_open && self.outline == Some(target) {
            return;
        }
        if let Some(morph) = &mut self.morph {
            if morph.opening {
                morph.to = target;
            }
            return;
        }
        if self.unified() && self.visible && self.outline.is_some_and(|r| r != target) {
            self.begin_morph(self.outline.unwrap(), target, true, 1.0);
        } else {
            let mut presented = target;
            if self.start_open {
                let offset = self.px(self.start_motion.offset_dip());
                presented.top += offset;
                presented.bottom += offset;
            }
            self.set_bounds(presented);
            self.outline = Some(target);
        }
    }

    fn unified(&self) -> bool {
        self.bottom() && self.dock.as_ref().is_some_and(Dock::shared)
    }

    fn launcher_rect(&self) -> RECT {
        let width = self.px(if self.start_open {
            self.start_width
        } else {
            layout::WIDTH
        });
        let height = self.px(self.window_h());
        let top = if self.start_open || self.bottom() {
            self.position.1 - height
        } else {
            self.position.1
        };
        RECT {
            left: self.position.0,
            top,
            right: self.position.0 + width,
            bottom: top + height,
        }
    }

    fn current_rect(&self) -> RECT {
        let mut rect = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut rect);
        }
        rect
    }

    fn set_bounds(&mut self, rect: RECT) {
        let _span = util::perf::Span::new("bounds");
        let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                rect.left,
                rect.top,
                width,
                height,
                SWP_NOACTIVATE,
            );
        }
        if self.start_open || self.unified() {
            let radius = if self.start_open {
                start::RADIUS
            } else {
                crate::morph::radius(self.morph_opacity())
            };
            crate::theme::shape(self.hwnd, width, height, radius, self.dpi);
        }
        self.renderer.resize(width.max(1) as u32, height.max(1) as u32);
        if self.morph.is_some()
            && let Some(dock) = &mut self.dock
        {
            dock.surface_rect(rect);
        }
    }

    fn set_collapsed_style(&self, collapsed: bool) {
        if !self.unified() && collapsed {
            return;
        }
        unsafe {
            let style = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(
                self.hwnd,
                GWL_EXSTYLE,
                if collapsed {
                    style | WS_EX_NOACTIVATE.0 as isize
                } else {
                    style & !(WS_EX_NOACTIVATE.0 as isize)
                },
            );
        }
    }

    fn morph_opacity(&self) -> f32 {
        self.morph.as_ref().map_or(if self.visible { 1.0 } else { 0.0 }, |m| {
            let t = self.morph_progress(m);
            m.from_opacity + ((if m.opening { 1.0 } else { 0.0 }) - m.from_opacity) * t
        })
    }

    fn morph_progress(&self, morph: &Morph) -> f32 {
        if crate::util::perf::hook_on("WISP_CAPTURE")
            && let Some(value) = crate::util::perf::hook("WISP_CAPTURE_MORPH")
            && let Ok(value) = value.parse::<f32>()
        {
            return value.clamp(0.0, 1.0);
        }
        (morph.start.elapsed().as_secs_f32() * 1000.0
            / crate::util::perf::slowdown()
            / if morph.opening {
                crate::morph::OPEN_MS
            } else {
                crate::morph::CLOSE_MS
            })
        .min(1.0)
    }

    fn begin_morph(&mut self, from: RECT, to: RECT, opening: bool, from_opacity: f32) {
        util::perf::event("morph_begin");
        self.morph = Some(Morph {
            from,
            to,
            start: Instant::now(),
            opening,
            from_opacity,
        });
        // Composition acrylic re-blurs on every resize. Use its solid tint during
        // motion, then restore the live material once the geometry settles.
        self.palette = crate::theme::apply_backdrop(
            self.hwnd,
            self.palette.text.r > 0.5,
            crate::config::Backdrop::Solid,
            true,
            &self.config.backdrop_color,
            self.config.material_options(),
        );
        self.morph.as_mut().unwrap().start = Instant::now();
        self.outline = Some(from);
        self.set_bounds(from);
    }

    fn step_morph(&mut self) {
        let _span = util::perf::Span::new("morph");
        let Some(morph) = &self.morph else {
            return;
        };
        util::perf::event("morph_frame");
        let tick = Instant::now();
        let elapsed = morph.start.elapsed().as_secs_f64() * 1000.0;
        let t = self.morph_progress(morph);
        let opening = morph.opening;
        let from = morph.from;
        let to = morph.to;
        if crate::util::perf::hook_on("WISP_CAPTURE")
            && crate::util::perf::hook("WISP_CAPTURE_SCENARIO").as_deref() == Some("typing")
            && opening
            && self.editor.text() != "typed"
        {
            self.editor.insert("typed");
            self.update_results();
            std::eprintln!("during animation query={} morph=true", self.editor.text());
        }
        let rect = crate::morph::bounds(from, self.morph.as_ref().map_or(to, |m| m.to), t);
        self.outline = Some(rect);
        self.set_bounds(rect);
        self.paint();
        if t >= 1.0 {
            self.morph = None;
            self.apply_theme();
            if !opening {
                self.outline = None;
                self.set_bounds(self.dock.as_ref().unwrap().resting_rect());
                if let Some(dock) = &mut self.dock {
                    dock.resume();
                }
            }
        }
        if crate::util::perf::hook_on("WISP_FRAME_TIMING") {
            std::eprintln!(
                "morph frame timestamp_ms={elapsed:.3} work_ms={:.3}",
                tick.elapsed().as_secs_f64() * 1000.0
            );
        }
    }

    fn update_results(&mut self) {
        let _span = util::perf::Span::new("search");
        let rows_before = self.window_rows();
        if self.start_home() {
            self.base_results.clear();
            self.file_results.clear();
            self.file_query = None;
            self.results.clear();
            self.selected = 0;
            self.scroll = 0;
            self.apply_size();
            self.invalidate();
            return;
        }
        if self.surface.is_some() {
            self.update_surface();
            if self.start_open || self.window_rows() != rows_before {
                self.apply_size();
            }
            return;
        }
        if let Some(scope) = &self.scope {
            let items = match (scope.drill, &self.list) {
                (Some(_), Some(list)) => list.clone(),
                (Some(drill), None) => vec![commands::loading(drill)],
                (None, _) => scope.actions.clone(),
            };
            let filter = self.editor.text().trim().to_lowercase();
            self.base_results = items
                .into_iter()
                .filter(|item| filter.is_empty() || item.title.to_lowercase().contains(&filter))
                .collect();
            self.file_query = None;
            self.file_results.clear();
            self.merge_results();
            self.selected = 0;
            self.scroll = 0;
            if self.start_open || self.window_rows() != rows_before {
                self.apply_size();
            }
            self.invalidate();
            return;
        }
        self.base_results = commands::search(self.editor.text(), &self.command_state);
        if !commands::is_window_query(self.editor.text()) {
            self.base_results.extend(self.surfaces.search(self.editor.text()));
            self.base_results.extend(
                self.searcher
                    .search(self.editor.text(), &self.apps, &self.history, &self.config),
            );
        }
        if let Some(armed) = &self.armed {
            for item in &mut self.base_results {
                if let Action::Command(command @ Command::Power(power)) = &item.action
                    && command == armed
                {
                    item.title = format!("Press Enter again to {}", power.title().to_lowercase());
                    item.subtitle = "Type anything to cancel".into();
                }
            }
        }

        let scope = if self.config.file_search {
            FileScope::of(self.editor.text(), &self.base_results)
        } else {
            FileScope::None
        };
        let (file_query, limit) = match scope {
            FileScope::None => (None, 0),
            FileScope::Mixed(query) => (Some(query), MIXED_FILE_RESULTS),
            FileScope::Only(query) => (Some(query), ONLY_FILE_RESULTS),
        };
        match file_query {
            None => {
                self.file_query = None;
                self.file_results.clear();
            }
            // Keep the previous file results until the new ones arrive, so the
            // results below the apps don't flicker on each keystroke.
            Some(query) if self.file_query.as_deref() != Some(query) => {
                let exclude = Arc::from(self.config.file_search_exclude.as_slice());
                self.file_search.search(query, limit, exclude);
                self.file_query = Some(query.to_string());
            }
            Some(_) => {}
        }

        self.merge_results();
        self.selected = 0;
        self.scroll = 0;
        if self.start_open || self.window_rows() != rows_before {
            self.apply_size();
        }
        self.invalidate();
    }

    /// Rebuilds `results` from `base_results` and `file_results`.
    fn merge_results(&mut self) {
        // A refresh must never retarget a focused inline command to a new app.
        if self.start_focus.is_some_and(|i| i >= start::INLINE_BASE) {
            self.start_focus = None;
        }
        self.results.clear();
        self.results.extend(self.base_results.iter().cloned());
        self.results.extend(self.file_results.iter().cloned());
        if crate::util::perf::hook_on("WISP_CAPTURE")
            && let Some(rows) = crate::util::perf::hook("WISP_CAPTURE_ROWS")
            && let Ok(rows) = rows.parse::<usize>()
        {
            self.results.truncate(rows);
        }
    }

    fn drain_file_results(&mut self) {
        let mut latest = None;
        while let Ok(results) = self.file_events.try_recv() {
            // Icons are cached even for results that are already outdated.
            self.icons.extend(results.icons);
            if let Some(hits) = results.hits
                && self.file_query.as_deref() == Some(results.query.as_str())
            {
                latest = Some(hits);
            }
        }
        let Some(hits) = latest else {
            self.invalidate();
            return;
        };

        let rows_before = self.window_rows();
        self.file_results = hits.into_iter().map(search::file_result).collect();
        self.merge_results();
        self.selected = self.selected.min(self.results.len().saturating_sub(1));
        self.move_selection(0, false);
        if self.start_open || self.window_rows() != rows_before {
            self.apply_size();
        }
        self.invalidate();
    }

    fn move_selection(&mut self, delta: isize, wrap: bool) {
        let len = self.results.len() as isize;
        if len == 0 {
            return;
        }
        let target = self.selected as isize + delta;
        self.selected = if wrap {
            target.rem_euclid(len)
        } else {
            target.clamp(0, len - 1)
        } as usize;
        let rows = self.visible_rows();
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + rows {
            self.scroll = self.selected + 1 - rows;
        }
        self.invalidate();
    }

    fn activate(&mut self, admin: bool) {
        let Some(result) = self.results.get(self.selected).cloned() else {
            return;
        };
        self.perform(&result.action, &result.title, result.kind, admin);
    }

    fn perform(&mut self, action: &Action, title: &str, kind: Kind, admin: bool) {
        let outcome = match action {
            Action::Launch {
                id,
                path,
                admin: as_admin,
            } => {
                let result = if let Some(app) = self.apps.iter().find(|app| &app.id == id) {
                    launch::indexed_app(app, admin || *as_admin)
                } else {
                    launch::app(id, path.as_deref(), admin || *as_admin)
                };
                if result.is_ok() {
                    self.history.record(id);
                    self.update_dock_apps();
                }
                result
            }
            Action::Copy(text) => system::set_clipboard(self.hwnd, text),
            Action::Open(target) => launch::open(target),
            Action::Shell { command, .. } if command.is_empty() => return,
            Action::Shell {
                command,
                admin: as_admin,
            } => launch::shell(command, admin || *as_admin),
            Action::Reveal(path) => launch::reveal(path),
            Action::None => return,
            Action::Command(command) => {
                self.run_command(command.clone());
                return;
            }
            Action::Surface(index) => {
                self.open_surface(*index);
                return;
            }
        };
        match outcome {
            Ok(()) => self.hide(),
            Err(e) if launch::is_cancelled(&e) => {}
            Err(e) => {
                crate::logging::error("launcher.action", format_args!("code={:#010X}", e.code().0));
                tray::notify(
                    self.hwnd,
                    &format!("Could not {} {}", kind.verb().to_lowercase(), title),
                    &e.message(),
                    true,
                );
            }
        }
    }

    /// Toggles and adjustments stay open and show the new state. Other
    /// commands close the launcher. Commands that lose work need a second Enter.
    fn run_command(&mut self, command: Command) {
        if let Command::Power(power) = command
            && power.needs_confirmation()
            && self.armed.as_ref() != Some(&command)
        {
            self.armed = Some(command);
            let selected = self.selected;
            self.update_results();
            self.move_selection(selected as isize, false);
            return;
        }
        self.armed = None;
        match command {
            Command::Quit => unsafe {
                let _ = PostMessageW(Some(self.hwnd), WM_APP_QUIT, WPARAM(0), LPARAM(0));
                return;
            },
            // These need the launcher's foreground rights, so they run here.
            Command::FocusWindow(hwnd) => {
                if let Err(error) = commands::open_windows::focus(hwnd) {
                    tray::notify(self.hwnd, "Wisp", &error, true);
                }
                self.hide();
                return;
            }
            Command::Power(power) => {
                self.hide();
                if let Err(error) = commands::power::run(power) {
                    tray::notify(self.hwnd, "Wisp", &error, true);
                }
                return;
            }
            _ => {}
        }
        // Show the new state now; the worker confirms it.
        surface::apply(&command, &mut self.command_state);
        match &command {
            Command::WifiPower(on) => {
                if let Some(Some(wifi)) = &mut self.command_state.wifi {
                    wifi.on = *on;
                    wifi.network = wifi.network.take().filter(|_| *on);
                }
            }
            Command::BluetoothPower(on) => {
                if let Some(Some(bluetooth)) = &mut self.command_state.bluetooth {
                    bluetooth.on = *on;
                    bluetooth.connected.retain(|_| *on);
                }
            }
            Command::Volume(percent) => {
                if let Some(Some(audio)) = &mut self.command_state.audio {
                    audio.volume = *percent;
                    audio.muted = false;
                }
            }
            Command::Mute(muted) => {
                if let Some(Some(audio)) = &mut self.command_state.audio {
                    audio.muted = *muted;
                }
            }
            Command::CloseWindow(hwnd) => self.command_state.windows.retain(|w| w.hwnd != *hwnd),
            _ => {}
        }
        let stays_open = command.stays_open();
        self.commands.run(command);
        self.commands_sent += 1;
        // Dock audio commands must not resize the shared launcher window.
        if !self.visible {
            if let Some(dock) = &self.dock {
                dock.update_sound(&self.command_state);
            }
            return;
        }
        if !stays_open {
            self.hide();
            return;
        }
        // Reload an open list, since the command may change it.
        if let Some(drill) = self.scope.as_ref().and_then(|s| s.drill) {
            self.list = None;
            self.commands.list(drill);
        }
        let selected = self.selected;
        self.update_results();
        self.move_selection(selected as isize, false);
    }

    /// Opens the selected result's list: a command's items or the result's actions.
    fn enter_scope(&mut self) {
        if self.scope.is_some() {
            return;
        }
        let Some(item) = self.results.get(self.selected) else {
            return;
        };
        if let Action::Surface(index) = item.action {
            self.open_surface(index);
            return;
        }
        let (title, drill, actions) = match item.drill {
            Some(drill) => {
                self.list = None;
                self.commands.list(drill);
                (drill.title().to_string(), Some(drill), Vec::new())
            }
            None => {
                let actions = item.actions();
                if actions.is_empty() {
                    return;
                }
                (item.title.clone(), None, actions)
            }
        };
        self.scope = Some(Scope {
            title,
            drill,
            actions,
            query: self.editor.text().to_string(),
        });
        self.editor.select_all();
        self.editor.insert("");
        self.update_results();
    }

    /// Returns to the results the list was opened from. Returns false if no list is open.
    fn leave_scope(&mut self) -> bool {
        let Some(scope) = self.scope.take() else {
            return false;
        };
        self.list = None;
        self.editor.select_all();
        self.editor.insert(&scope.query);
        self.update_results();
        true
    }

    fn drain_command_events(&mut self) {
        let mut changed = false;
        while let Ok(event) = self.command_events.try_recv() {
            match event {
                commands::Event::TopbarStatus(audio, wifi, ran) => {
                    if ran >= self.commands_sent
                        && let Some(dock) = &self.dock
                    {
                        if let Some(wifi) = &wifi {
                            dock.confirm_wifi(wifi.on);
                        }
                        self.command_state.audio = Some(audio.clone());
                        self.command_state.wifi = Some(wifi.clone());
                        dock.update_topbar_status(audio, wifi);
                    }
                }
                commands::Event::State(mut state, icons, ran) => {
                    if ran >= self.commands_sent
                        && let Some(dock) = &self.dock
                        && let Some(Some(wifi)) = &state.wifi
                    {
                        dock.confirm_wifi(wifi.on);
                    }

                    // The worker read this before running every command sent,
                    // so keep what the launcher shows for those.
                    if ran < self.commands_sent {
                        let shown = &self.command_state;
                        state.wifi = shown.wifi.clone();
                        state.bluetooth = shown.bluetooth.clone();
                        state.audio = shown.audio.clone();
                        state.outputs = shown.outputs.clone();
                        for session in &mut state.sessions {
                            if let Some(old) = shown.sessions.iter().find(|s| s.pid == session.pid) {
                                session.volume = old.volume;
                                session.muted = old.muted;
                            }
                        }
                    }
                    changed |= !icons.is_empty() || self.command_state != *state;
                    self.command_state = *state;
                    self.icons.extend(icons);
                }
                commands::Event::List(drill, items) => {
                    if self.scope.as_ref().is_some_and(|s| s.drill == Some(drill)) {
                        self.list = Some(items);
                        changed = true;
                    }
                }
                commands::Event::Notch(id, track) => {
                    // Settings may have changed while the worker was reading.
                    let track = track.filter(|track| {
                        !commands::media::excluded(
                            &track.media.source,
                            &track.media.app,
                            &self.config.music_excluded_apps,
                        )
                    });
                    if let Some(dock) = &self.dock
                        && dock.topbar_matches(id)
                    {
                        dock.update_topbar(id, track);
                        continue;
                    }
                    if let Some(notch) = &mut self.notch
                        && notch.matches(id)
                    {
                        notch.update(track);
                    }
                }
                commands::Event::Control(hardware) => {
                    if let Some(dock) = &self.dock {
                        dock.update_control_hardware(hardware);
                    }
                }
                commands::Event::Failed(error) => tray::notify(self.hwnd, "Wisp", &error, true),
            }
        }
        if changed && let Some(dock) = &self.dock {
            dock.update_sound(&self.command_state);
        }
        if changed && self.visible {
            let selected = self.selected;
            self.update_results();
            self.move_selection(selected as isize, false);
        }
    }

    fn reveal_selected(&mut self) {
        if let Some(path) = self.results.get(self.selected).and_then(ResultItem::path)
            && launch::reveal(path).is_ok()
        {
            self.hide();
        }
    }

    fn copy_selected_path(&mut self) {
        if let Some(path) = self.results.get(self.selected).and_then(ResultItem::path) {
            let _ = system::set_clipboard(self.hwnd, path);
        }
    }

    fn drain_index_events(&mut self) {
        let mut apps_changed = false;
        let mut icons_changed = false;
        while let Ok(event) = self.index_events.try_recv() {
            match event {
                IndexEvent::Apps(apps) => {
                    self.app_generation = self.app_generation.wrapping_add(1);
                    self.apps = apps;
                    apps_changed = true;
                }
                IndexEvent::Icons(icons) => {
                    self.icons.extend(icons);
                    icons_changed = true;
                }
            }
        }
        if apps_changed || icons_changed {
            self.update_dock_apps();
        }
        if apps_changed && self.visible {
            let selected = self.selected;
            self.update_results();
            self.move_selection(selected as isize, false);
        } else if icons_changed {
            self.invalidate();
        }
    }

    fn on_key(&mut self, vk: VIRTUAL_KEY) -> bool {
        let down = |key: VIRTUAL_KEY| unsafe { GetKeyState(key.0 as i32) < 0 };
        let (ctrl, shift) = (down(VK_CONTROL), down(VK_SHIFT));
        if let Some((menu, selected)) = self.footer_menu {
            self.footer_hover = None;
            let count = menu.actions().len();
            match vk {
                VK_UP => self.footer_menu = Some((menu, (selected + count - 1) % count)),
                VK_DOWN | VK_TAB => self.footer_menu = Some((menu, (selected + 1) % count)),
                VK_ESCAPE => self.footer_menu = None,
                VK_RETURN => self.activate_footer_row(menu, selected),
                _ => return false,
            }
            self.invalidate();
            return true;
        }
        if self.start_open
            && (self.surface.is_none() || vk == VK_F6 || self.start_focus.is_some())
            && self.on_start_key(vk, shift)
        {
            return true;
        }
        if self.surface.is_some() && self.on_surface_key(vk, ctrl, shift) {
            self.restart_caret();
            return true;
        }
        let page = self.list_rows() as isize;
        let mut edited = false;
        match vk {
            VK_ESCAPE => {
                if !self.leave_scope() {
                    self.hide();
                }
                return true;
            }
            VK_RETURN => {
                self.activate(ctrl && shift);
                return true;
            }
            VK_UP => self.move_on_screen(-1, true),
            VK_DOWN => self.move_on_screen(1, true),
            VK_TAB if shift => {
                self.leave_scope();
            }
            VK_TAB => self.enter_scope(),
            VK_PRIOR => self.move_on_screen(-page, false),
            VK_NEXT => self.move_on_screen(page, false),
            VK_LEFT => self.editor.left(ctrl, shift),
            VK_RIGHT => self.editor.right(ctrl, shift),
            VK_HOME => self.editor.home(shift),
            VK_END => self.editor.end(shift),
            VK_BACK if self.scope.is_some() && self.editor.text().is_empty() => {
                self.leave_scope();
            }
            VK_BACK => edited = self.editor.backspace(ctrl),
            VK_DELETE => edited = self.editor.delete(ctrl),
            VK_A if ctrl => self.editor.select_all(),
            VK_C if ctrl && shift => self.copy_selected_path(),
            VK_C if ctrl => self.copy(),
            VK_X if ctrl && self.editor.has_selection() => {
                self.copy();
                self.editor.insert("");
                edited = true;
            }
            VK_V if ctrl => {
                if let Some(text) = system::get_clipboard(self.hwnd) {
                    self.editor.insert(&text.replace(['\r', '\n'], " "));
                    edited = true;
                }
            }
            VK_E if ctrl && shift => {
                self.reveal_selected();
                return true;
            }
            _ => return false,
        }
        if edited {
            self.armed = None;
            self.update_results();
        }
        self.restart_caret();
        self.invalidate();
        true
    }

    fn copy(&mut self) {
        let text = if self.editor.has_selection() {
            Some(self.editor.selected_text())
        } else {
            match self.results.get(self.selected) {
                Some(ResultItem {
                    kind: Kind::Calc,
                    title,
                    ..
                }) => Some(title.as_str()),
                _ => None,
            }
        };
        if let Some(text) = text {
            let _ = system::set_clipboard(self.hwnd, text);
        }
    }

    fn on_char(&mut self, unit: u16) {
        // Space presses the focused widget, unless it is part of a filter.
        if unit == u16::from(b' ') && self.surface.is_some() && self.editor.text().is_empty() {
            self.activate_surface();
            return;
        }
        if self.editor.push_utf16(unit) {
            self.footer_menu = None;
            self.footer_hover = None;
            self.start_hover = None;
            self.start_focus = None;
            self.armed = None;
            self.update_results();
            self.restart_caret();
        }
    }

    fn row_at(&self, y: i32) -> Option<usize> {
        let y = y as f32 * 96.0 / self.dpi as f32;
        if self.start_open {
            start::row_at(y, self.visible_rows(), self.window_h()).map(|row| self.scroll + row)
        } else {
            layout::row_at(y, self.visible_rows(), self.window_h(), self.bottom()).map(|row| self.scroll + row)
        }
    }

    /// Moves the selection by `delta` rows on screen. At the bottom of the
    /// screen the list runs upward, so up moves to the next result.
    fn move_on_screen(&mut self, delta: isize, wrap: bool) {
        let delta = if self.bottom() { -delta } else { delta };
        self.move_selection(delta, wrap);
    }

    fn content_point(&self, x: i32, y: i32) -> (i32, i32) {
        if self.morph.is_none() {
            return (x, y);
        }
        let r = self.current_rect();
        let scale = self.dpi as f32 / 96.0;
        let (ox, oy) = crate::morph::content_offset(
            (r.right - r.left) as f32,
            (r.bottom - r.top) as f32,
            layout::WIDTH * scale,
            self.window_h() * scale,
        );
        ((x as f32 - ox).round() as i32, (y as f32 - oy).round() as i32)
    }

    fn on_mouse_move(&mut self, x: i32, y: i32) {
        // Windows also sends mouse moves when the launcher moves under the cursor.
        // Compare screen positions so resizing and morphing keep the selection.
        let Some(position) = cursor_pos() else {
            return;
        };
        if !mouse_moved(&mut self.last_mouse, position) {
            return;
        }
        if self.start_open {
            unsafe {
                let _ = TrackMouseEvent(&mut TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: self.hwnd,
                    dwHoverTime: 0,
                });
            }
            let hover = self
                .start_hit(x, y)
                .or_else(|| self.inline_hit(x, y).map(|i| start::INLINE_BASE + i));
            let row = self.footer_menu.and_then(|(menu, _)| self.footer_row_at(menu, x, y));
            if (hover, row) != (self.start_hover, self.footer_hover) {
                self.start_hover = hover;
                self.footer_hover = row;
                self.invalidate();
            }
            if self.footer_menu.is_some() {
                return;
            }
        }
        if self.surface.is_some() {
            if let Some(index) = self.widget_at(x, y) {
                self.focus_widget(index);
            }
            return;
        }
        if let Some(row) = self.row_at(y)
            && row != self.selected
        {
            self.selected = row;
            self.start_focus = None;
            self.invalidate();
        }
    }

    fn start_hit(&self, x: i32, y: i32) -> Option<usize> {
        if !self.start_open {
            return None;
        }
        let scale = 96. / self.dpi as f32;
        let (x, y) = (x as f32 * scale, y as f32 * scale);
        let pins = if self.start_home() {
            self.pins.apps(&self.apps).len()
        } else {
            0
        };
        start::hit(x, y, self.start_width, self.window_h(), pins)
    }

    fn start_action(&mut self, index: usize) {
        if index < start::CAPACITY {
            if let Some(app) = self.pins.apps(&self.apps).get(index).map(|app| (*app).clone()) {
                self.perform(
                    &Action::Launch {
                        id: app.id,
                        path: app.path,
                        admin: false,
                    },
                    &app.name,
                    Kind::App,
                    false,
                );
            }
            return;
        }
        let footer = index - start::CAPACITY;
        if footer == 1 {
            self.hide();
            self.on_tray_command(tray::CMD_SETTINGS);
            return;
        }
        let menu = if footer == 0 {
            start::FooterMenu::Profile
        } else {
            start::FooterMenu::Power
        };
        self.footer_menu = Some((menu, 0));
        self.footer_hover = None;
        self.invalidate();
    }

    fn start_power(&mut self, power: commands::power::Power) {
        if power.needs_confirmation() {
            self.leave_scope();
            self.leave_surface();
            self.editor = crate::editor::Editor::default();
            self.editor.insert(power.title());
            self.armed = None;
            self.start_focus = None;
            self.update_results();
            self.apply_size();
        }
        self.run_command(Command::Power(power));
    }

    fn activate_footer_row(&mut self, menu: start::FooterMenu, index: usize) {
        self.footer_menu = None;
        self.footer_hover = None;
        self.start_hover = None;
        match menu.actions()[index] {
            start::FooterAction::Power(power) => self.start_power(power),
            start::FooterAction::Quit => self.run_command(Command::Quit),
            start::FooterAction::Remote(mode) => {
                match crate::remote_mode::save(&util::data_dir().join("remote-mode.txt"), mode) {
                    Ok(()) => {
                        let previous = self.remote_active();
                        self.remote_mode = mode;
                        if mode == crate::remote_mode::Mode::Auto {
                            self.remote_rdp = self.query_remote_session();
                        }
                        self.apply_remote_change(previous);
                    }
                    Err(error) => tray::notify(self.hwnd, "Remote mode", &error.to_string(), true),
                }
                self.paint();
            }
        }
    }

    fn footer_row_at(&self, menu: start::FooterMenu, x: i32, y: i32) -> Option<usize> {
        let scale = 96. / self.dpi as f32;
        let (x, y) = (x as f32 * scale, y as f32 * scale);
        (0..menu.actions().len()).find(|&i| {
            let (left, top, right, bottom) = menu.row(i, self.start_width, self.window_h());
            x >= left && x < right && y >= top && y < bottom
        })
    }

    fn on_start_key(&mut self, vk: VIRTUAL_KEY, shift: bool) -> bool {
        if !self.start_home() {
            // F6 visits the persistent utility controls without taking Tab away
            // from the existing search drill-down and surface keyboard model.
            if vk == VK_F6 || self.start_focus.is_some() && vk == VK_TAB {
                let actions = self.inline_actions().len();
                self.start_focus = start::utility_focus(self.start_focus, actions, shift);
                self.invalidate();
                return true;
            }
            if vk == VK_RETURN
                && self
                    .start_focus
                    .is_some_and(|i| (start::CAPACITY..start::INLINE_BASE).contains(&i))
            {
                self.start_action(self.start_focus.unwrap());
                return true;
            }
            if self.start_focus.is_some() && vk == VK_ESCAPE {
                self.start_focus = None;
                self.invalidate();
                return true;
            }
            if !matches!(vk, VK_SHIFT | VK_CONTROL | VK_MENU) {
                self.start_focus = None;
            }
            return false;
        }
        let pins = self.pins.apps(&self.apps).len();
        if vk == VK_RETURN {
            if let Some(index) = self.start_focus {
                self.start_action(index);
            }
            return true;
        }
        let current = self.start_focus;
        self.start_focus = match vk {
            VK_TAB => {
                let sequence: Vec<_> = (0..pins).chain(start::CAPACITY..start::CAPACITY + 3).collect();
                match current.and_then(|i| sequence.iter().position(|&item| item == i)) {
                    Some(i) if shift => i.checked_sub(1).map(|i| sequence[i]),
                    Some(i) => sequence.get(i + 1).copied(),
                    None if shift => sequence.last().copied(),
                    None => sequence.first().copied(),
                }
            }
            VK_DOWN if current.is_none() => (pins > 0).then_some(0).or(Some(start::CAPACITY)),
            VK_LEFT | VK_RIGHT | VK_UP | VK_DOWN if current.is_some_and(|i| i < pins) => {
                let step = match vk {
                    VK_LEFT => -1,
                    VK_RIGHT => 1,
                    VK_UP => -6,
                    _ => 6,
                };
                let next = current.unwrap() as isize + step;
                if next >= 0 && (next as usize) < pins {
                    Some(next as usize)
                } else if vk == VK_DOWN {
                    Some(start::CAPACITY)
                } else {
                    current
                }
            }
            VK_LEFT | VK_RIGHT if current.is_some() => {
                let index = current.unwrap();
                Some(if vk == VK_LEFT {
                    index.saturating_sub(1).max(start::CAPACITY)
                } else {
                    (index + 1).min(start::CAPACITY + 2)
                })
            }
            VK_UP if current.is_some() => pins.checked_sub(1),
            _ => return false,
        };
        self.invalidate();
        true
    }

    fn inline_app(&self) -> Option<&crate::indexer::AppEntry> {
        if !self.start_open || self.start_home() || self.surface.is_some() || self.footer_menu.is_some() {
            return None;
        }
        let result = self.results.get(self.selected)?;
        match &result.action {
            Action::Launch { id, .. } if result.kind == Kind::App => self.apps.iter().find(|app| &app.id == id),
            _ => None,
        }
    }

    fn inline_actions(&self) -> Vec<start::AppMenuAction> {
        self.inline_app()
            .map(|_| start::app_menu_actions(false, false).into_iter().take(3).collect())
            .unwrap_or_default()
    }

    fn inline_hit(&self, x: i32, y: i32) -> Option<usize> {
        let scale = 96. / self.dpi as f32;
        let (x, y) = (x as f32 * scale, y as f32 * scale);
        (0..self.inline_actions().len()).find(|&i| {
            let (l, t, r, b) = start::inline_rect(i, self.start_width, self.start_height);
            x >= l && x < r && y >= t && y < b
        })
    }

    fn prepare_app_effect(&mut self, app: AppEntry, action: start::AppMenuAction) -> Option<StartAppEffect> {
        if !self.visible || self.start_motion.closing() || self.start_menu_active || self.start_native_action {
            return None;
        }
        let snapshot = StartAppMenu {
            generation: self.app_generation,
            owner: self.hwnd,
            session: self.start_session,
            app,
            actions: vec![action],
            anchor: POINT::default(),
            dock_pins: self.dock.as_ref().map(Dock::app_pins),
        };
        self.apply_start_app_menu(snapshot, Some(0))
    }

    fn prepare_inline_action(&mut self, index: usize) -> Option<StartAppEffect> {
        self.prepare_app_effect(self.inline_app()?.clone(), *self.inline_actions().get(index)?)
    }

    fn start_app_at(&self, x: i32, y: i32) -> Option<(AppEntry, start::AppMenuAction)> {
        if !self.start_open || self.surface.is_some() || self.footer_menu.is_some() {
            return None;
        }
        if self.start_home() {
            self.start_hit(x, y).and_then(|index| {
                self.pins
                    .apps(&self.apps)
                    .get(index)
                    .map(|app| ((*app).clone(), start::AppMenuAction::Open))
            })
        } else {
            let result = self.results.get(self.row_at(y)?)?;
            let (id, action) = start::result_app_action(result, false)?;
            self.apps
                .iter()
                .find(|app| app.id == id)
                .cloned()
                .map(|app| (app, action))
        }
    }

    fn prepare_start_app_menu(&mut self, x: i32, y: i32) -> Option<StartAppMenu> {
        if !self.start_open
            || !self.visible
            || self.start_motion.closing()
            || self.start_menu_active
            || self.start_native_action
        {
            return None;
        }
        let pins = self.pins.apps(&self.apps);
        let id = if self.start_home() {
            self.start_hit(x, y)
                .and_then(|index| pins.get(index))
                .map(|app| app.id.clone())
        } else {
            self.row_at(y)
                .and_then(|row| self.results.get(row))
                .and_then(|result| start::result_app_action(result, false).map(|(id, _)| id.to_string()))
        }?;
        let app = self.apps.iter().find(|app| app.id == id)?.clone();
        let pinned = pins.iter().any(|app| app.id == id);
        let mut anchor = POINT { x, y };
        unsafe {
            let _ = ClientToScreen(self.hwnd, &mut anchor);
        }
        self.start_menu_active = true;
        Some(StartAppMenu {
            generation: self.app_generation,
            owner: self.hwnd,
            session: self.start_session,
            actions: start::app_menu_actions(
                pinned,
                self.dock
                    .as_ref()
                    .map_or_else(|| dock::saved_start_pinned(&id), |dock| dock.app_pins().contains(&id)),
            ),
            dock_pins: self.dock.as_ref().map(Dock::app_pins),
            app,
            anchor,
        })
    }

    fn apply_start_app_menu(&mut self, snapshot: StartAppMenu, picked: Option<usize>) -> Option<StartAppEffect> {
        self.start_menu_active = false;
        if self.hwnd != snapshot.owner
            || self.start_session != snapshot.session
            || self.app_generation != snapshot.generation
            || !self.start_open
            || !self.visible
        {
            return None;
        }
        let (app, action) = start::resolve_app_action(&snapshot.app.id, &self.apps, &snapshot.actions, picked)?;
        let app = app.clone();
        if !start::app_snapshot_matches(
            &snapshot.app,
            &app,
            (snapshot.session, snapshot.generation),
            (self.start_session, self.app_generation),
        ) {
            return None;
        }
        self.start_native_action = true;
        Some(StartAppEffect {
            owner: self.hwnd,
            session: self.start_session,
            app,
            action,
            dock_pins: snapshot.dock_pins,
        })
    }

    fn on_click(&mut self, x: i32, y: i32) {
        if let Some((menu, _)) = self.footer_menu.take() {
            self.footer_hover = None;
            if let Some(row) = self.footer_row_at(menu, x, y) {
                self.activate_footer_row(menu, row);
                self.invalidate();
                return;
            }
            self.invalidate();
            if let Some(index) = self.start_hit(x, y).filter(|&index| index >= start::CAPACITY) {
                let same_button = match menu {
                    start::FooterMenu::Profile => start::CAPACITY,
                    start::FooterMenu::Power => start::CAPACITY + 2,
                };
                if index != same_button {
                    self.start_action(index);
                }
            }
            return;
        }
        if let Some(index) = self.start_hit(x, y) {
            self.start_focus = Some(index);
            self.start_action(index);
            return;
        }
        if self.surface.is_some() {
            self.on_surface_click(x, y);
            return;
        }
        if let Some(row) = self.row_at(y) {
            self.selected = row;
            self.activate(false);
        }
    }

    fn restart_caret(&mut self) {
        self.caret_visible = true;
        unsafe {
            SetTimer(Some(self.hwnd), CARET_TIMER, GetCaretBlinkTime(), None);
        }
        self.invalidate();
    }

    fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    fn paint(&mut self) {
        let _span = util::perf::Span::new("paint");
        let opacity = self.morph_opacity();
        let content_size = self.morph.as_ref().map(|_| (layout::WIDTH, self.window_h()));
        let text = self.editor.text();
        let pins = self.start_home().then(|| self.pins.apps(&self.apps));
        let start_actions: Vec<_> = self
            .inline_actions()
            .into_iter()
            .map(|action| {
                (
                    action,
                    self.inline_app()
                        .is_some_and(|app| start::disabled_reason(action, app).is_none()),
                )
            })
            .collect();
        let frame = Frame {
            start: self.start_open,
            start_actions: &start_actions,
            pins: pins.as_deref(),
            start_focus: self.start_focus,
            footer_menu: self.footer_menu,
            remote_mode: self.remote_mode,
            start_hover: self.start_hover,
            footer_hover: self.footer_hover,
            palette: &self.palette,
            text,
            caret: util::utf16_offset(text, self.editor.caret()),
            anchor: util::utf16_offset(text, self.editor.anchor()),
            caret_visible: self.caret_visible,
            placeholder: if self.scope.is_some() || self.surface.is_some() {
                "Filter"
            } else if self.start_open {
                "Search"
            } else {
                PLACEHOLDER
            },
            chip: match (&self.scope, &self.surface) {
                (_, Some(open)) => self.surfaces.specs.get(open.index).map(|spec| spec.title.as_str()),
                (Some(scope), None) => Some(scope.title.as_str()),
                (None, None) => None,
            },
            results: &self.results,
            selected: self.selected,
            scroll: self.scroll,
            rows: self.visible_rows(),
            icons: &self.icons,
            surface: self.surface.as_ref().map(|open| SurfaceFrame {
                widgets: &open.widgets,
                rects: &open.rects,
                focus: open.focus,
                stop: open.stop,
                scroll: open.scroll,
                rows: self.list_rows(),
            }),
            bottom: self.bottom(),
            // Start fades the complete native surface, including its backdrop.
            opacity: if self.start_open {
                1.
            } else {
                crate::morph::fades(opacity).1
            },
            dock_opacity: crate::morph::dock_opacity(opacity, self.unified(), self.start_open),
            content_size,
            shared: self.unified(),
            radius: if self.start_open {
                start::RADIUS
            } else {
                crate::morph::radius(opacity)
            },
        };
        if let Err(e) = self.renderer.draw(self.hwnd, &frame, |rt| {
            if frame.dock_opacity > 0.0
                && let Some(dock) = &mut self.dock
            {
                let size = unsafe { rt.GetSize() };
                let rect = dock.resting_rect();
                let scale = self.dpi as f32 / 96.0;
                let dock_height = (rect.bottom - rect.top) as f32 / scale;
                let (x, y) = crate::morph::content_offset(
                    size.width,
                    size.height,
                    (rect.right - rect.left) as f32 / scale,
                    dock_height,
                );
                unsafe {
                    rt.SetTransform(&windows_numerics::Matrix3x2::translation(x, y));
                }
                let result = dock.draw_on(rt);
                unsafe {
                    rt.SetTransform(&windows_numerics::Matrix3x2::identity());
                }
                result
            } else {
                Ok(())
            }
        }) {
            eprintln!("could not draw launcher: {e}");
        }
    }

    fn on_tray_command(&mut self, cmd: u32) {
        match cmd {
            tray::CMD_OPEN => self.show(),
            tray::CMD_RESCAN => {
                self.indexer.rescan();
                self.last_scan = Instant::now();
            }
            // Without the settings app, open the config file instead.
            tray::CMD_SETTINGS if launch::settings(false).is_ok() => {}
            tray::CMD_SETTINGS | tray::CMD_CONFIG => {
                let path = Config::path();
                let path = path.to_string_lossy();
                // Notepad can edit the config if Windows cannot open .toml files.
                if launch::open(&path).is_err() {
                    let _ = launch::notepad(&path);
                }
            }
            tray::CMD_AUTOSTART => {
                if let Err(e) = system::set_autostart(!system::autostart_enabled()) {
                    tray::notify(self.hwnd, "Could not change Start with Windows", &e.message(), true);
                }
            }
            // Defer destruction until this handler releases the launcher borrow.
            // WM_DESTROY needs to borrow the launcher for shutdown.
            tray::CMD_QUIT => unsafe {
                let _ = PostMessageW(Some(self.hwnd), WM_APP_QUIT, WPARAM(0), LPARAM(0));
            },
            _ => {}
        }
    }

    /// Reloads the surface library, reporting new problems with the user's files.
    fn load_surfaces(&mut self) {
        let _span = util::perf::Span::new("surfaces");
        self.surfaces = Library::load();
        let new: Vec<&String> = self
            .surfaces
            .errors
            .iter()
            .filter(|e| !self.surface_errors.contains(e))
            .collect();
        if !new.is_empty() {
            let message = new.iter().map(|e| e.as_str()).collect::<Vec<_>>().join("\n");
            tray::notify(self.hwnd, "Wisp surfaces", &message, true);
        }
        self.surface_errors = self.surfaces.errors.clone();
    }

    fn open_surface(&mut self, index: usize) {
        if self.surfaces.specs.get(index).is_none() {
            return;
        }
        self.leave_scope();
        let rows_before = self.window_rows();
        self.surface = Some(OpenSurface {
            index,
            widgets: Vec::new(),
            rects: Vec::new(),
            focus: 0,
            stop: 0,
            scroll: 0.0,
            query: self.editor.text().to_string(),
        });
        self.editor.select_all();
        self.editor.insert("");
        self.results.clear();
        self.update_surface();
        if self.start_open || self.window_rows() != rows_before {
            self.apply_size();
        }
        self.commands.refresh();
        unsafe {
            SetTimer(Some(self.hwnd), SURFACE_TIMER, SURFACE_REFRESH_MS, None);
        }
        self.invalidate();
    }

    /// Returns to the results the surface was opened from. Returns false if no surface is open.
    fn leave_surface(&mut self) -> bool {
        let Some(open) = self.surface.take() else {
            return false;
        };
        unsafe {
            let _ = KillTimer(Some(self.hwnd), SURFACE_TIMER);
        }
        self.editor.select_all();
        self.editor.insert(&open.query);
        self.update_results();
        true
    }

    /// Resolves the open surface against the latest state, keeping the focus
    /// on the same widget.
    fn update_surface(&mut self) {
        let Some(open) = &mut self.surface else {
            return;
        };
        let Some(spec) = self.surfaces.specs.get(open.index) else {
            return;
        };
        let focused = open
            .widgets
            .get(open.focus)
            .map(|w| (w.id.clone(), w.command_at(open.stop).cloned()));
        open.widgets = surface::resolve(spec, &self.command_state, self.editor.text());
        open.rects = nav::layout(
            &open.widgets,
            if self.start_open {
                (self.start_width - 2. * layout::PAD).max(1.)
            } else {
                crate::render::surface_width()
            },
        );
        let same = focused
            .as_ref()
            .and_then(|(id, _)| open.widgets.iter().position(|w| &w.id == id));
        let moved = match same {
            Some(index) => {
                open.focus = index;
                // Options can reorder, such as the default output moving first.
                let widget = &open.widgets[index];
                if let Some(stop) = focused.and_then(|(_, command)| widget.stop_of(&command?)) {
                    open.stop = stop;
                }
                open.stop = open.stop.min(widget.stops().saturating_sub(1));
                false
            }
            None => {
                open.focus = surface::first_control(&open.widgets);
                open.stop = open.widgets.get(open.focus).map_or(0, Widget::first_stop);
                true
            }
        };
        self.results.clear();
        // Keep a scroll position the user chose unless the focus moved.
        if moved {
            self.scroll_surface_to_focus();
        } else {
            self.clamp_surface_scroll();
        }
        self.invalidate();
    }

    fn clamp_surface_scroll(&mut self) {
        let view_h = self.view_height();
        if let Some(open) = &mut self.surface {
            let max = (nav::content_height(&open.rects) - view_h).max(0.0);
            open.scroll = open.scroll.clamp(0.0, max);
        }
    }

    fn view_height(&self) -> f32 {
        self.list_rows() as f32 * layout::ROW_H
    }

    fn scroll_surface_to_focus(&mut self) {
        let view_h = self.view_height();
        if let Some(open) = &mut self.surface {
            let max = (nav::content_height(&open.rects) - view_h).max(0.0);
            if let Some(rect) = open.rects.get(open.focus) {
                open.scroll = nav::scroll_to(rect, open.scroll, view_h);
            }
            open.scroll = open.scroll.clamp(0.0, max);
        }
    }

    fn focus_widget(&mut self, index: usize) {
        let Some(open) = &mut self.surface else {
            return;
        };
        if index == open.focus || index >= open.widgets.len() {
            return;
        }
        open.focus = index;
        open.stop = open.widgets[index].first_stop();
        self.scroll_surface_to_focus();
        self.invalidate();
    }

    /// Handles keys while a surface is open. Returns false for keys that edit the filter.
    fn on_surface_key(&mut self, vk: VIRTUAL_KEY, ctrl: bool, shift: bool) -> bool {
        let Some(open) = &self.surface else {
            return false;
        };
        let count = open.widgets.len();
        let focus = open.focus;
        match vk {
            VK_ESCAPE => {
                self.leave_surface();
            }
            VK_BACK if self.editor.text().is_empty() => {
                self.leave_surface();
            }
            VK_RETURN => self.activate_surface(),
            VK_TAB if count > 0 => {
                let next = if shift { focus + count - 1 } else { focus + 1 } % count;
                self.focus_widget(next);
            }
            VK_UP | VK_DOWN => {
                let direction = if vk == VK_UP { Direction::Up } else { Direction::Down };
                if let Some(next) = nav::neighbour(&open.rects, focus, direction) {
                    self.focus_widget(next);
                }
            }
            VK_LEFT | VK_RIGHT => {
                let step: i32 = if vk == VK_LEFT { -1 } else { 1 };
                let Some(widget) = open.widgets.get(focus) else {
                    return true;
                };
                match &widget.input {
                    Input::Slider { value, target, .. } => {
                        let by = if ctrl { 1 } else { surface::STEP } as i32;
                        let value = (*value as i32 + step * by).clamp(0, 100) as u32;
                        self.run_command(target.command(value));
                    }
                    Input::Choice { .. } | Input::Buttons { .. }
                        if (0..widget.stops() as i32).contains(&(open.stop as i32 + step)) =>
                    {
                        if let Some(open) = &mut self.surface {
                            open.stop = (open.stop as i32 + step) as usize;
                        }
                        self.invalidate();
                    }
                    _ => {
                        let direction = if step < 0 { Direction::Left } else { Direction::Right };
                        if let Some(next) = nav::neighbour(&open.rects, focus, direction) {
                            self.focus_widget(next);
                        }
                    }
                }
            }
            VK_PRIOR | VK_NEXT => {
                let view_h = self.view_height();
                if let Some(open) = &mut self.surface {
                    let max = (nav::content_height(&open.rects) - view_h).max(0.0);
                    let by = if vk == VK_PRIOR { -view_h } else { view_h };
                    open.scroll = (open.scroll + by).clamp(0.0, max);
                }
                self.invalidate();
            }
            _ => return false,
        }
        true
    }

    /// Runs what the focused widget does, such as switching Wi-Fi off.
    fn activate_surface(&mut self) {
        let Some(open) = &self.surface else {
            return;
        };
        let Some(widget) = open.widgets.get(open.focus) else {
            return;
        };
        if let Some(action) = widget.activate(open.stop) {
            let label = widget.label.clone();
            self.perform(&action, &label, Kind::Command, false);
        }
    }

    /// Converts window pixels to DIPs relative to the surface's top-left corner.
    fn surface_point(&self, x: i32, y: i32) -> Option<(f32, f32)> {
        let open = self.surface.as_ref()?;
        let scale = 96.0 / self.dpi as f32;
        let (ox, oy) = if self.start_open {
            (layout::PAD, start::LIST_TOP)
        } else {
            crate::render::surface_origin(self.bottom())
        };
        let (x, y) = (x as f32 * scale, y as f32 * scale);
        if y < oy || y > oy + self.view_height() {
            return None;
        }
        Some((x - ox, y - oy + open.scroll))
    }

    fn widget_at(&self, x: i32, y: i32) -> Option<usize> {
        let (sx, sy) = self.surface_point(x, y)?;
        self.surface.as_ref()?.rects.iter().position(|r| r.contains(sx, sy))
    }

    fn on_surface_click(&mut self, x: i32, y: i32) {
        let Some(index) = self.widget_at(x, y) else {
            return;
        };
        self.focus_widget(index);
        let scale = 96.0 / self.dpi as f32;
        let stop = self
            .renderer
            .stop_at(x as f32 * scale, y as f32 * scale)
            .filter(|(widget, _)| *widget == index)
            .map(|(_, stop)| stop);
        let Some(point) = self.surface_point(x, y) else {
            return;
        };
        let Some(open) = &mut self.surface else {
            return;
        };
        match &open.widgets[index].input {
            Input::Slider { target, .. } => {
                let command = target.command(nav::slider_value(&open.rects[index], point.0));
                self.run_command(command);
            }
            Input::Choice { .. } | Input::Buttons { .. } => {
                if let Some(stop) = stop {
                    open.stop = stop;
                    self.activate_surface();
                }
            }
            _ => self.activate_surface(),
        }
    }

    fn on_surface_wheel(&mut self, up: bool) {
        let view_h = self.view_height();
        if let Some(open) = &mut self.surface {
            let max = (nav::content_height(&open.rects) - view_h).max(0.0);
            open.scroll = (open.scroll + if up { -48.0 } else { 48.0 }).clamp(0.0, max);
        }
        self.invalidate();
    }

    fn surface_tick(&mut self) {
        if self.visible && self.surface.is_some() {
            self.commands.refresh();
            // The clock changes without new state.
            self.update_surface();
        }
    }

    fn shutdown(&mut self) {
        unregister_remote_notifications();
        self.start_motion.settle();
        self.start_motion_tick = None;
        self.start_activating = false;
        unsafe {
            let _ = KillTimer(Some(self.hwnd), dock::POPUP_FADE_TIMER);
            let _ = KillTimer(Some(self.hwnd), START_ACTIVATE_TIMER);
        }
        self.win_key = None;
        self.notch = None;
        self.dock = None;
        if let Err(e) = TaskbarHider::configure(&mut self.taskbar, self.hwnd, false) {
            eprintln!("could not restore the taskbar: {e}");
        }
        // Removed last, so Windows works the work area out again from the
        // restored taskbar and the remaining app bars. Waits, so the space
        // is back before Wisp exits.
        if let Some(space) = self.dock_space.take() {
            space.close();
        }
        unsafe {
            let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_ID);
        }
        tray::remove(self.hwnd);
    }
}

fn loword(v: usize) -> u32 {
    (v & 0xFFFF) as u32
}

fn mouse_pos(lparam: LPARAM) -> (i32, i32) {
    (
        (lparam.0 & 0xFFFF) as i16 as i32,
        ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
    )
}

fn cursor_pos() -> Option<(i32, i32)> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) }.ok().map(|()| (point.x, point.y))
}

fn mouse_moved(last: &mut Option<(i32, i32)>, position: (i32, i32)) -> bool {
    last.replace(position).is_some_and(|previous| previous != position)
}

// Run OLE without borrowing the launcher: its modal loop dispatches window messages.
fn run_drag(hwnd: HWND, payload: Payload) {
    unsafe {
        let _ = ReleaseCapture();
    }
    let Ok(data) = payload.data_object() else {
        return;
    };
    let _ = payload.set_image(&data);
    DRAGGING.with(|d| d.set(true));
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
    let effect = unsafe { SHDoDragDrop(Some(hwnd), &data, None, payload.effect()) };
    // Restore the same query on cancellation. A completed drop uses the normal
    // hide path, including the return morph into the dock.
    with_launcher(|l| {
        if effect.is_ok_and(|effect| effect != DROPEFFECT_NONE) {
            l.hide();
        }
        if l.visible || l.morph.is_some() {
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                if l.visible {
                    let _ = SetForegroundWindow(hwnd);
                    let _ = SetFocus(Some(hwnd));
                }
            }
            l.invalidate();
        }
    });
    DRAGGING.with(|d| d.set(false));
}

/// Both native modal dispatch and ShellExecute run with no Launcher borrow.
/// Index/status callbacks can update the app while the owned snapshot is open.
fn run_start_app_menu(x: i32, y: i32) {
    let Some(snapshot) = with_launcher(|launcher| launcher.prepare_start_app_menu(x, y)).flatten() else {
        return;
    };
    crate::logging::event("start.app.menu", format_args!("stage=open"));
    let Some(appearance) = with_launcher(start_app_menu_appearance) else {
        return;
    };
    let config_stamp = with_launcher(|l| l.config_mtime).flatten();
    let rows: Vec<_> = snapshot
        .actions
        .iter()
        .map(|action| start::app_menu::Row {
            label: action.label(),
            reason: start::disabled_reason(*action, &snapshot.app),
            glyph: match action {
                start::AppMenuAction::Open => "\u{E8E5}",
                start::AppMenuAction::Administrator => "\u{EA18}",
                start::AppMenuAction::Location => "\u{E8B7}",
                start::AppMenuAction::SetPinned(_) | start::AppMenuAction::SetDockPinned(_) => "\u{E718}",
                start::AppMenuAction::CopyPath => "\u{E8C8}",
                start::AppMenuAction::AppSettings => "\u{E713}",
                start::AppMenuAction::Uninstall => "\u{E74D}",
            },
        })
        .collect();
    let picked = start::app_menu::pick(
        snapshot.owner,
        snapshot.anchor,
        rows,
        appearance.clone(),
        || {
            with_launcher(|l| {
                l.hwnd == snapshot.owner
                    && l.start_session == snapshot.session
                    && l.app_generation == snapshot.generation
                    && l.start_open
                    && l.visible
                    && l.start_menu_active
                    && !l.start_motion.closing()
                    && l.config_mtime == config_stamp
                    && l.config.backdrop == appearance.backdrop
                    && l.config.backdrop_color == appearance.color
                    && l.config.material_options() == appearance.material_options
                    && l.remote_active() == appearance.remote
                    && start_app_menu_dark(&l.palette) == appearance.dark
                    && l.apps.iter().find(|a| a.id == snapshot.app.id).is_some_and(|a| {
                        start::app_snapshot_matches(
                            &snapshot.app,
                            a,
                            (snapshot.session, snapshot.generation),
                            (l.start_session, l.app_generation),
                        )
                    })
            })
            .unwrap_or(false)
        },
        dispatch_ui_message,
    );
    let effect = with_launcher(|launcher| launcher.apply_start_app_menu(snapshot, picked)).flatten();
    crate::logging::event(
        "start.app.menu",
        format_args!("stage=close selected={}", picked.is_some()),
    );
    let Some(effect) = effect else {
        with_launcher(Launcher::hide_after_deactivation);
        return;
    };
    run_start_app_effect(effect);
}

fn start_app_menu_appearance(launcher: &mut Launcher) -> start::app_menu::Appearance {
    let animations = dock::client_animations_enabled();
    start_app_menu_policy(
        &launcher.palette,
        &launcher.config,
        launcher.remote_active(),
        animations,
    )
}

fn start_app_menu_policy(
    palette: &Palette,
    config: &Config,
    remote: bool,
    animations: bool,
) -> start::app_menu::Appearance {
    start::app_menu::Appearance {
        dark: start_app_menu_dark(palette),
        backdrop: config.backdrop,
        color: config.backdrop_color.clone(),
        material_options: config.material_options(),
        remote,
        opening: animations && config.dock_open_ms != 0,
        closing: animations && config.dock_open_ms != 0 && config.dock_close_ms != 0,
    }
}

/// Custom color and high contrast can override the configured theme. Capture
/// and validate the palette Start actually uses, without querying OS policy.
fn start_app_menu_dark(palette: &Palette) -> bool {
    palette.text.r > 0.5
}

/// All message pumps must clear the pacing flag before dispatch. Otherwise a
/// nested modal loop can consume the sole frame message and stall future frames.
fn dispatch_ui_message(msg: &MSG) {
    FRAME_PENDING.with(|slot| {
        if let Some(pending) = slot.borrow().as_ref() {
            clear_frame_pending(msg.message, pending);
        }
    });
    unsafe {
        let _ = TranslateMessage(msg);
        DispatchMessageW(msg);
    }
}

fn clear_frame_pending(message: u32, pending: &AtomicBool) {
    if message == WM_APP_FRAME {
        pending.store(false, Ordering::Release);
    }
}

fn run_start_inline_action(index: usize) {
    if let Some(effect) = with_launcher(|l| l.prepare_inline_action(index)).flatten() {
        run_start_app_effect(effect);
    }
}

fn run_start_app_effect(effect: StartAppEffect) {
    crate::logging::event("start.app.action", format_args!("action={}", effect.action.code()));
    let result = match effect.action {
        start::AppMenuAction::Open => launch::indexed_app(&effect.app, false),
        start::AppMenuAction::Administrator => launch::indexed_app(&effect.app, true),
        start::AppMenuAction::Location => effect.app.path.as_deref().map_or(Ok(()), launch::reveal),
        start::AppMenuAction::SetPinned(pinned) => {
            let state = with_launcher(|launcher| (launcher.pins.clone(), launcher.apps.clone()));
            if let Some((mut pins, apps)) = state {
                let result = pins
                    .set_pinned(&effect.app.id, pinned, &apps)
                    .map_err(|error| windows::core::Error::new(windows::Win32::Foundation::E_FAIL, error.to_string()));
                if result.is_ok() {
                    with_launcher(|launcher| {
                        launcher.pins = pins;
                        launcher.start_focus = None;
                        launcher.invalidate();
                    });
                }
                result
            } else {
                Ok(())
            }
        }
        start::AppMenuAction::SetDockPinned(pinned) => {
            dock::set_start_pin(effect.dock_pins.as_ref(), effect.app.clone(), pinned)
                .map_err(|error| windows::core::Error::new(windows::Win32::Foundation::E_FAIL, error.to_string()))
        }
        start::AppMenuAction::CopyPath => effect
            .app
            .path
            .as_deref()
            .map_or(Ok(()), |path| system::set_clipboard(effect.owner, path)),
        start::AppMenuAction::AppSettings | start::AppMenuAction::Uninstall => launch::open("ms-settings:appsfeatures"),
    };
    let notification = with_launcher(|launcher| {
        launcher.start_native_action = false;
        let current = launcher.hwnd == effect.owner && launcher.start_session == effect.session;
        match result {
            Ok(()) => {
                if matches!(
                    effect.action,
                    start::AppMenuAction::Open | start::AppMenuAction::Administrator
                ) {
                    launcher.history.record(&effect.app.id);
                    launcher.update_dock_apps();
                }
                if current
                    && !matches!(
                        effect.action,
                        start::AppMenuAction::SetPinned(_)
                            | start::AppMenuAction::SetDockPinned(_)
                            | start::AppMenuAction::CopyPath
                    )
                {
                    launcher.hide();
                } else if current {
                    launcher.invalidate();
                    launcher.hide_after_deactivation();
                }
            }
            Err(error) if launch::is_cancelled(&error) => {
                if current {
                    launcher.hide_after_deactivation();
                }
            }
            Err(error) => {
                crate::logging::error(
                    "start.app.action",
                    format_args!("action={} code={:#010X}", effect.action.code(), error.code().0),
                );
                if current {
                    return Some(error.message());
                }
            }
        }
        None
    })
    .flatten();
    if let Some(message) = notification {
        tray::notify(effect.owner, "Could not complete app action", &message, true);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if DRAGGING.with(Cell::get)
        && matches!(
            msg,
            WM_ACTIVATE
                | WM_HOTKEY
                | WM_APP_SHOW
                | start::TOGGLE
                | dock::WM_SHOW_LAUNCHER
                | WM_KEYDOWN
                | WM_SYSKEYDOWN
                | WM_CHAR
                | WM_MOUSEMOVE
                | WM_LBUTTONDOWN
                | WM_LBUTTONUP
                | WM_RBUTTONUP
                | WM_MOUSEWHEEL
                | WM_MOUSEHWHEEL
        )
    {
        return LRESULT(0);
    }
    let start_closing = with_launcher(|l| l.start_open && l.start_motion.closing()).unwrap_or(false);
    if start_closing {
        if msg == WM_MOUSEACTIVATE {
            return LRESULT(MA_NOACTIVATE as isize);
        }
        if msg == WM_NCHITTEST {
            return LRESULT(HTTRANSPARENT as isize);
        }
        if matches!(
            msg,
            WM_KEYDOWN
                | WM_SYSKEYDOWN
                | WM_CHAR
                | WM_MOUSEMOVE
                | WM_LBUTTONDOWN
                | WM_LBUTTONUP
                | WM_MBUTTONDOWN
                | WM_MBUTTONUP
                | WM_RBUTTONUP
                | WM_MOUSEWHEEL
                | WM_MOUSEHWHEEL
        ) {
            return LRESULT(0);
        }
    }
    let dock_message = with_launcher(|l| {
        l.unified()
            && (Dock::owns_message(msg, wparam)
                || !l.visible
                    && l.morph.is_none()
                    && matches!(
                        msg,
                        WM_PAINT
                            | WM_MOUSEMOVE
                            | WM_MOUSELEAVE
                            | WM_CAPTURECHANGED
                            | WM_CANCELMODE
                            | WM_MOUSEACTIVATE
                            | WM_LBUTTONDOWN
                            | WM_LBUTTONUP
                            | WM_MBUTTONUP
                            | WM_RBUTTONUP
                            | WM_MOUSEWHEEL
                            | WM_MOUSEHWHEEL
                            | WM_NCACTIVATE
                    ))
    })
    .unwrap_or(false);
    if dock_message {
        return with_launcher(|l| l.dock.as_ref().unwrap().dispatch(msg, wparam, lparam)).unwrap_or(LRESULT(0));
    }
    match msg {
        // DWM needs the frame styles for acrylic. Make the full window client area.
        WM_NCCALCSIZE if wparam.0 != 0 => return LRESULT(0),
        // Keep the activation state but never paint the caption. It lands on
        // the client area, as a "Wisp" title strip across the top of the dock.
        // This also covers the dock's menus: while one is open the launcher is
        // borrowed, so the dock's own handler above never sees the message.
        WM_NCACTIVATE => return unsafe { DefWindowProcW(hwnd, msg, wparam, LPARAM(-1)) },
        WM_NCHITTEST => return LRESULT(HTCLIENT as isize),
        WM_HOTKEY => {
            util::perf::input("hotkey_to_content");
            with_launcher(Launcher::toggle);
        }
        WM_APP_SHOW => {
            with_launcher(Launcher::show);
        }
        #[cfg(feature = "perf-trace")]
        WM_TIMER if wparam.0 == 97 && crate::util::perf::hook_on("WISP_CAPTURE") => {
            with_launcher(|l| {
                thread_local! { static STEP: Cell<usize> = const { Cell::new(0) }; }
                let step = STEP.with(|s| {
                    let n = s.get();
                    s.set(n + 1);
                    n
                });
                let scenario = crate::util::perf::hook("WISP_PERF_SCENARIO").unwrap_or_default();
                if scenario == "typing" {
                    if !l.visible {
                        l.show();
                    }
                    util::perf::input("key_to_content");
                    l.on_char(if step.is_multiple_of(6) { 8 } else { 101 });
                    l.paint();
                } else if scenario == "slide" {
                    if let Some(dock) = &mut l.dock {
                        dock.capture_slide();
                    }
                } else if !scenario.starts_with("idle") {
                    if l.visible {
                        l.hide();
                    } else {
                        if scenario == "hidden"
                            && let Some(dock) = &mut l.dock
                        {
                            dock.hide();
                        }
                        util::perf::input("hotkey_to_content");
                        l.show();
                    }
                }
            });
        }
        WM_TIMER if wparam.0 == 98 && crate::util::perf::hook_on("WISP_CAPTURE") => {
            with_launcher(|l| {
                unsafe {
                    let _ = KillTimer(Some(l.hwnd), 98);
                }
                let scenario = crate::util::perf::hook("WISP_CAPTURE_SCENARIO").unwrap_or_default();
                std::eprintln!(
                    "scenario={scenario} visible={} morph={} return_to_dock={}",
                    l.visible,
                    l.morph.is_some(),
                    l.morphed
                );
                if scenario == "slide" {
                    if let Some(dock) = &mut l.dock {
                        dock.capture_slide();
                    }
                } else if scenario == "typing" {
                    std::eprintln!("retained query={}", l.editor.text());
                } else {
                    l.hide();
                    std::eprintln!(
                        "dismiss visible={} morph={} hwnd_visible={}",
                        l.visible,
                        l.morph.is_some(),
                        unsafe { IsWindowVisible(l.hwnd).as_bool() }
                    );
                }
            });
        }
        WM_TIMER if wparam.0 == REEL_TIMER && crate::util::perf::hook_on("WISP_CAPTURE") => {
            with_launcher(|l| {
                thread_local! { static STEP: Cell<u32> = const { Cell::new(0) }; }
                let step = STEP.with(|s| {
                    s.set(s.get() + 1);
                    s.get()
                });
                if step == 1 {
                    let interval = (REEL_MS as f32 * crate::util::perf::slowdown()) as u32;
                    unsafe {
                        SetTimer(Some(l.hwnd), REEL_TIMER, interval, None);
                    }
                }
                // Steps of REEL_MS: the hidden dock slides in, grows into the
                // launcher, takes a query, then shrinks back into the dock.
                match step {
                    1 => {
                        if let Some(dock) = &mut l.dock {
                            dock.capture_reveal();
                        }
                    }
                    19 => l.show(),
                    // The whole query at once: partial queries can list
                    // apps by their paths on this PC.
                    29 => {
                        for unit in b"note" {
                            l.on_char((*unit).into());
                        }
                        l.paint();
                    }
                    56 => l.hide(),
                    81 => unsafe {
                        unregister_remote_notifications();
                        let _ = DestroyWindow(hwnd);
                    },
                    _ => {}
                }
            });
        }
        WM_TIMER if wparam.0 == 99 && crate::util::perf::hook_on("WISP_CAPTURE") => unsafe {
            unregister_remote_notifications();
            let _ = DestroyWindow(hwnd);
        },
        WM_APP_QUIT => unsafe {
            unregister_remote_notifications();
            let _ = DestroyWindow(hwnd);
        },
        crate::remote_session::WM_WTSSESSION_CHANGE => {
            if crate::remote_session::relevant(REMOTE_SESSION_ID.get(), lparam.0 as u32, wparam.0) {
                with_launcher_or_defer(DEFER_REMOTE, Launcher::refresh_remote_session);
            }
        }
        WM_APP_RELOAD => {
            with_launcher_or_defer(DEFER_RELOAD, Launcher::reload_config_if_changed);
        }
        WM_APP_FRAME => {
            with_launcher(|l| {
                l.step_morph();
                if let Some(dock) = &mut l.dock {
                    dock.frame();
                }
            });
        }
        WM_APP_INDEX => with_launcher_or_defer(DEFER_INDEX, Launcher::drain_index_events),
        WM_APP_FILES => with_launcher_or_defer(DEFER_FILES, Launcher::drain_file_results),
        WM_APP_COMMANDS => with_launcher_or_defer(DEFER_COMMANDS, Launcher::drain_command_events),
        WM_ACTIVATE if loword(wparam.0) == WA_INACTIVE => {
            with_launcher_or_defer(DEFER_HIDE, Launcher::hide_after_deactivation)
        }
        WM_DPICHANGED => {
            with_launcher(|l| l.on_dpi_changed(loword(wparam.0)));
            return LRESULT(0);
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let key = VIRTUAL_KEY(wparam.0 as u16);
            // Bit 30 is set when the key was already down. A held Enter must not confirm an action.
            if key == VK_RETURN && lparam.0 & (1 << 30) != 0 {
                return LRESULT(0);
            }
            let inline = with_launcher(|l| {
                (key == VK_RETURN && l.start_open)
                    .then_some(l.start_focus)
                    .flatten()
                    .and_then(|i| i.checked_sub(start::INLINE_BASE))
            })
            .flatten();
            if let Some(index) = inline {
                run_start_inline_action(index);
                return LRESULT(0);
            }
            let menu_point = with_launcher(|l| {
                if !l.start_open
                    || l.footer_menu.is_some()
                    || l.surface.is_some()
                    || !(key == VK_APPS || key == VK_F10 && unsafe { GetKeyState(VK_SHIFT.0 as i32) < 0 })
                {
                    return None;
                }
                let (x, y) = if l.start_home() {
                    let index = l.start_focus.filter(|&index| index < l.pins.apps(&l.apps).len())?;
                    let bounds = start::tile(index, l.start_width, l.start_height);
                    ((bounds.0 + bounds.2) / 2., (bounds.1 + bounds.3) / 2.)
                } else {
                    l.inline_app()?;
                    start::result_menu_anchor(l.selected.checked_sub(l.scroll)?, l.start_width)
                };
                Some((l.px(x), l.px(y)))
            })
            .flatten();
            if let Some((x, y)) = menu_point {
                run_start_app_menu(x, y);
                return LRESULT(0);
            }
            let app_action = with_launcher(|l| {
                if !l.start_open || l.footer_menu.is_some() || l.surface.is_some() {
                    return None;
                }
                let ctrl_shift = unsafe { GetKeyState(VK_CONTROL.0 as i32) < 0 && GetKeyState(VK_SHIFT.0 as i32) < 0 };
                let action = match key {
                    VK_RETURN if !l.start_home() => start::result_app_action(l.results.get(l.selected)?, ctrl_shift)?.1,
                    VK_RETURN => {
                        if ctrl_shift {
                            start::AppMenuAction::Administrator
                        } else {
                            start::AppMenuAction::Open
                        }
                    }
                    VK_E if ctrl_shift => start::AppMenuAction::Location,
                    VK_C if ctrl_shift => start::AppMenuAction::CopyPath,
                    _ => return None,
                };
                let app = if l.start_home() {
                    let index = l.start_focus.filter(|&index| index < start::CAPACITY)?;
                    l.pins.apps(&l.apps).get(index).map(|app| (*app).clone())?
                } else {
                    if l.start_focus.is_some() {
                        return None;
                    }
                    l.inline_app()?.clone()
                };
                Some(l.prepare_app_effect(app, action))
            })
            .flatten();
            if let Some(effect) = app_action {
                if let Some(effect) = effect {
                    run_start_app_effect(effect);
                }
                return LRESULT(0);
            }
            if with_launcher(|l| l.on_key(key)) == Some(true) {
                return LRESULT(0);
            }
        }
        WM_CHAR => {
            util::perf::input("key_to_content");
            with_launcher(|l| l.on_char(wparam.0 as u16));
            return LRESULT(0);
        }
        // Suppress the Windows beep for Alt shortcuts handled by the launcher.
        WM_SYSCHAR => return LRESULT(0),
        WM_LBUTTONDOWN => {
            let point = mouse_pos(lparam);
            with_launcher(|l| {
                let point = l.content_point(point.0, point.1);
                l.drag_consumed = false;
                l.start_inline_press = l
                    .inline_hit(point.0, point.1)
                    .and_then(|i| Some((l.inline_app()?.clone(), *l.inline_actions().get(i)?, l.app_generation)));
                l.drag_press = if l.visible && l.surface.is_none() && l.morph.is_none() {
                    (l.footer_menu.is_none())
                        .then(|| l.row_at(point.1))
                        .flatten()
                        .and_then(|row| l.results.get(row))
                        .and_then(Payload::from_result)
                        .map(|payload| (point, payload))
                } else {
                    None
                };
                if l.drag_press.is_some() {
                    unsafe {
                        SetCapture(hwnd);
                    }
                }
            });
        }
        WM_MOUSELEAVE => {
            with_launcher(|l| {
                if l.start_hover.take().is_some() | l.footer_hover.take().is_some() {
                    l.invalidate();
                }
            });
        }
        WM_CAPTURECHANGED => {
            with_launcher(|l| {
                l.drag_press = None;
                l.start_inline_press = None;
            });
        }
        WM_MOUSEMOVE => {
            let (x, y) = mouse_pos(lparam);
            let payload = with_launcher(|l| {
                if wparam.0 & 1 == 0 {
                    l.drag_press = None;
                    l.start_inline_press = None;
                } else if let Some((start, _)) = l.drag_press.as_ref() {
                    let size = unsafe {
                        (
                            GetSystemMetricsForDpi(SM_CXDRAG, l.dpi),
                            GetSystemMetricsForDpi(SM_CYDRAG, l.dpi),
                        )
                    };
                    if drag::crossed_threshold(*start, (x, y), size) {
                        l.drag_consumed = true;
                        return l.drag_press.take().map(|(_, payload)| payload);
                    }
                }
                let (x, y) = l.content_point(x, y);
                l.on_mouse_move(x, y);
                None
            })
            .flatten();
            if let Some(payload) = payload {
                run_drag(hwnd, payload);
                return LRESULT(0);
            }
        }
        WM_LBUTTONUP => {
            let (x, y) = mouse_pos(lparam);
            let inline = with_launcher(|l| {
                let (app, action, generation) = l.start_inline_press.take()?;
                let index = l.inline_hit(x, y)?;
                (!l.drag_consumed
                    && l.drag_press.is_none()
                    && generation == l.app_generation
                    && l.inline_app() == Some(&app)
                    && start::inline_identity_matches(
                        &app.id,
                        action,
                        &l.inline_app()?.id,
                        *l.inline_actions().get(index)?,
                    ))
                .then_some(index)
            })
            .flatten();
            if let Some(index) = inline {
                run_start_inline_action(index);
                return LRESULT(0);
            }
            let effect = with_launcher(|l| {
                let consumed = std::mem::take(&mut l.drag_consumed);
                let captured = l.drag_press.take().is_some();
                if captured {
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                }
                let mut rect = RECT::default();
                let inside = !captured
                    || unsafe { GetClientRect(hwnd, &mut rect).is_ok() && PtInRect(&rect, POINT { x, y }).as_bool() };
                if !consumed && inside {
                    let (x, y) = l.content_point(x, y);
                    if let Some((app, action)) = l.start_app_at(x, y) {
                        return l.prepare_app_effect(app, action);
                    }
                    l.on_click(x, y);
                }
                None
            })
            .flatten();
            if let Some(effect) = effect {
                run_start_app_effect(effect);
            }
        }
        WM_RBUTTONUP => {
            let (x, y) = mouse_pos(lparam);
            let point = with_launcher(|l| l.content_point(x, y)).unwrap_or((x, y));
            run_start_app_menu(point.0, point.1);
        }
        WM_MOUSEWHEEL => {
            let delta = (wparam.0 >> 16) as u16 as i16;
            with_launcher(|l| {
                if l.start_open {
                    let mut cursor = POINT::default();
                    unsafe {
                        let _ = GetCursorPos(&mut cursor);
                        let _ = ScreenToClient(l.hwnd, &mut cursor);
                    }
                    if cursor.y as f32 * 96. / l.dpi as f32 >= l.start_height - start::FOOTER - start::ACTIONS {
                        return;
                    }
                }
                if l.surface.is_some() {
                    l.on_surface_wheel(delta > 0);
                } else {
                    l.move_on_screen(if delta > 0 { -1 } else { 1 }, false);
                }
            });
        }
        WM_TIMER if wparam.0 == START_ACTIVATE_TIMER => {
            with_launcher(Launcher::finish_start_activation);
        }
        WM_TIMER if wparam.0 == dock::POPUP_FADE_TIMER => {
            with_launcher(Launcher::tick_start_motion);
        }
        WM_TIMER if wparam.0 == SURFACE_TIMER => {
            with_launcher(Launcher::surface_tick);
        }
        WM_TIMER if wparam.0 == CARET_TIMER => {
            with_launcher(|l| {
                l.caret_visible = !l.caret_visible;
                l.invalidate();
            });
        }
        WM_PAINT => unsafe {
            let mut ps = PAINTSTRUCT::default();
            BeginPaint(hwnd, &mut ps);
            if with_launcher(Launcher::paint).is_none() {
                DEFERRED.with(|d| d.set(d.get() | DEFER_PAINT));
            }
            let _ = EndPaint(hwnd, &ps);
            return LRESULT(0);
        },
        WM_ERASEBKGND => return LRESULT(1),
        WM_SETTINGCHANGE => {
            // Explorer waits for every window while it broadcasts this, and
            // the taskbar hider asks Explorer for its state: let it go first.
            unsafe {
                let _ = ReplyMessage(LRESULT(0));
            }
            with_launcher(|l| {
                l.on_taskbar_message(msg);
                l.apply_theme();
            });
        }
        WM_DISPLAYCHANGE | taskbar::WM_TASKBAR_SHOW | taskbar::WM_RECOVERY_DONE => {
            unsafe {
                let _ = ReplyMessage(LRESULT(0));
            }
            with_launcher(|l| {
                l.on_taskbar_message(msg);
                if msg == WM_DISPLAYCHANGE {
                    l.sync_shell();
                    if l.start_open {
                        l.place_on_active_monitor();
                    }
                }
            });
        }
        dock::WM_SHOW_LAUNCHER | start::TOGGLE => {
            with_launcher(Launcher::toggle_start);
        }
        dock::topbar::REFRESH => {
            with_launcher(|l| {
                if l.dock.as_ref().is_some_and(|dock| dock.topbar_polling(wparam.0)) {
                    l.commands.refresh_notch(wparam.0, &l.config.music_excluded_apps);
                    l.commands.refresh_topbar_status();
                }
            });
        }
        dock::topbar::BATTERY_SETTINGS => {
            // Snapshot validity only; ShellExecute may dispatch callbacks and
            // must run without borrowing either the launcher or the bar.
            let valid =
                with_launcher(|l| l.dock.as_ref().is_some_and(|dock| dock.topbar_matches(wparam.0))).unwrap_or(false);
            if valid && let Err(error) = crate::launch::open("ms-settings:batterysaver") {
                eprintln!("Open battery settings: {error}");
            }
        }
        dock::topbar::ACTION => {
            with_launcher(|l| {
                if let Some(command) = l.dock.as_ref().and_then(|dock| dock.take_topbar_action(wparam.0)) {
                    l.run_command(command);
                }
            });
        }
        crate::notch::REFRESH => {
            with_launcher(|l| {
                if l.notch.as_ref().is_some_and(|notch| notch.matches(wparam.0)) {
                    l.commands.refresh_notch(wparam.0, &l.config.music_excluded_apps);
                }
            });
        }
        crate::notch::ACTION => {
            with_launcher(|l| {
                if let Some(notch) = &mut l.notch
                    && notch.matches(wparam.0)
                    && let Some(control) = notch.take_action()
                {
                    l.commands.run(Command::Media(control));
                    l.commands_sent += 1;
                }
            });
        }
        dock::sound::WM_SOUND_REFRESH => {
            with_launcher(|l| {
                if let Some(dock) = &l.dock
                    && dock.sound_window(wparam.0).is_some()
                {
                    dock.update_sound(&l.command_state);
                    l.commands.refresh_sound();
                    if lparam.0 == 1 {
                        l.commands.refresh();
                        let _ = l.commands.control(commands::control::Request::Batteries);
                    }
                }
            });
        }
        dock::sound::WM_SOUND_ACTION => {
            with_launcher(|l| {
                use dock::sound::Action;
                let Some(window) = l.dock.as_ref().and_then(|dock| dock.sound_window(wparam.0)) else {
                    return;
                };
                let Some(action) = dock::sound::take_action(window) else {
                    return;
                };
                let result = match action {
                    Action::Run(command) => {
                        l.run_command(command);
                        return;
                    }
                    Action::Hardware(request) => {
                        if !l.commands.control(request) {
                            tray::notify(
                                l.hwnd,
                                "Control Center",
                                "Device controls are busy. Try again in a moment.",
                                true,
                            );
                        }
                        return;
                    }
                    Action::Settings(uri) => launch::open(uri),
                    Action::Project => launch::project(),
                    Action::QuickSettings => launch::quick_settings(),
                    Action::WispSettings => launch::settings(false),
                };
                if let Err(error) = result {
                    crate::logging::error("controls.action", format_args!("code={:#010X}", error.code().0));
                    tray::notify(l.hwnd, "Control Center", &error.message(), true);
                }
            });
        }
        dock::WM_DOCK_SHOW_TASKBAR => {
            with_launcher(Launcher::peek_taskbar);
        }
        WM_TIMER if wparam.0 == PEEK_TIMER => {
            with_launcher(Launcher::peek_tick);
        }
        WM_TIMER if wparam.0 == TASKBAR_TIMER => {
            unsafe {
                let _ = KillTimer(Some(hwnd), TASKBAR_TIMER);
            }
            with_launcher(Launcher::sync_taskbar);
        }
        WM_ENDSESSION if wparam.0 != 0 => taskbar::emergency_restore(),
        tray::WM_TRAY => match loword(lparam.0 as usize) {
            WM_LBUTTONUP => {
                with_launcher(Launcher::show);
            }
            WM_RBUTTONUP | WM_CONTEXTMENU => {
                // Release the launcher borrow before the menu's modal loop handles messages.
                let Some(hotkey) = with_launcher(|l| l.registered_hotkey.clone().unwrap_or_default()) else {
                    return LRESULT(0);
                };
                let cmd = tray::show_menu(hwnd, &hotkey, system::autostart_enabled());
                with_launcher(|l| l.on_tray_command(cmd));
            }
            _ => {}
        },
        // Keep Wisp running after Alt+F4 so the shortcut can reopen the launcher.
        WM_CLOSE => {
            with_launcher(Launcher::hide);
            return LRESULT(0);
        }
        WM_DESTROY => unsafe {
            with_launcher(Launcher::shutdown);
            PostQuitMessage(0);
        },
        m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => {
            // Explorer removes tray icons when it restarts.
            with_launcher(|l| {
                tray::add(l.hwnd, l.tray_icon, &l.tooltip());
                // The new Explorer shows its taskbar again.
                l.on_taskbar_message(m);
            });
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    #[test]
    fn app_menu_snapshot_tracks_shared_material_overrides_for_reload_cancellation() {
        let palette = super::Palette::new(true, true);
        let mut config = super::Config {
            backdrop: crate::config::Backdrop::Transparent,
            ..Default::default()
        };
        let before = super::start_app_menu_policy(&palette, &config, false, true);
        config.backdrop_transparency = Some(100);
        let clear = super::start_app_menu_policy(&palette, &config, false, true);
        assert_ne!(before, clear);
        assert_eq!(clear.material_options, config.material_options());
        config.backdrop_blur = Some(true);
        let blurred = super::start_app_menu_policy(&palette, &config, true, true);
        assert_ne!(clear, blurred);
        assert_eq!(blurred.material_options, config.for_remote(true).material_options());
    }

    #[test]
    fn app_menu_uses_resolved_palette_when_color_or_high_contrast_overrides_theme() {
        // A light Custom color under configured Dark uses dark text; the
        // opposite high-contrast palette can use light text under Light.
        let light_override = super::Palette::new(false, false);
        let dark_override = super::Palette::new(true, false);
        let mut config = super::Config {
            theme: super::Theme::Dark,
            backdrop: crate::config::Backdrop::Color,
            backdrop_color: "#FFFFFF".into(),
            ..Default::default()
        };
        let captured_light = super::start_app_menu_policy(&light_override, &config, false, true).dark;
        config.theme = super::Theme::Light;
        let captured_dark = super::start_app_menu_policy(&dark_override, &config, false, true).dark;
        assert!(!captured_light);
        assert!(captured_dark);
        assert_eq!(super::start_app_menu_dark(&light_override), captured_light);
        assert_eq!(super::start_app_menu_dark(&dark_override), captured_dark);
        assert_ne!(super::start_app_menu_dark(&dark_override), captured_light);
        assert_ne!(super::start_app_menu_dark(&light_override), captured_dark);
    }
    #[test]
    fn nested_dispatch_clears_only_the_pending_frame_message() {
        let pending = super::AtomicBool::new(true);
        super::clear_frame_pending(super::WM_APP_INDEX, &pending);
        assert!(pending.load(super::Ordering::Acquire));
        super::clear_frame_pending(super::WM_APP_FRAME, &pending);
        assert!(!pending.load(super::Ordering::Acquire));
        pending.store(true, super::Ordering::Release);
        super::clear_frame_pending(super::WM_APP_FRAME, &pending);
        assert!(!pending.load(super::Ordering::Acquire));
    }
    use super::mouse_moved;

    #[test]
    fn stationary_cursor_keeps_selection_across_layout_changes() {
        let mut last = Some((800, 600));
        // Growing or moving the launcher changes content coordinates, not these.
        for _ in 0..3 {
            assert!(!mouse_moved(&mut last, (800, 600)));
        }
    }

    #[test]
    fn real_cursor_movement_allows_hover() {
        let mut last = Some((-800, 600));
        assert!(mouse_moved(&mut last, (-799, 600)));
        assert!(!mouse_moved(&mut last, (-799, 600)));
        assert!(mouse_moved(&mut last, (-799, 601)));
        assert!(mouse_moved(&mut last, (-800, 600)));
    }

    #[test]
    fn unknown_cursor_position_needs_a_baseline() {
        let mut last = None;
        assert!(!mouse_moved(&mut last, (800, 600)));
        assert_eq!(last, Some((800, 600)));
        assert!(mouse_moved(&mut last, (801, 600)));
    }
}
