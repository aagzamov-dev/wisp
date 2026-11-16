//! Independent, nonactivating app dock on the message-loop thread.

#[path = "dock/desktops.rs"]
mod desktops;
#[path = "dock/logic.rs"]
mod logic;
#[path = "dock/render.rs"]
pub(crate) mod render;
#[path = "dock/space.rs"]
pub mod space;

#[path = "dock/calendar.rs"]
mod calendar;
#[path = "dock/calendar_model.rs"]
mod calendar_model;
#[path = "dock/pins.rs"]
mod pins;
#[path = "dock/status.rs"]
mod status;
#[path = "dock/topbar.rs"]
pub mod topbar;
#[path = "dock/topbar_blur.rs"]
pub(crate) mod topbar_blur;
#[path = "dock/topbar_space.rs"]
mod topbar_space;
#[path = "dock/tray.rs"]
mod tray;

/// Reserved height in DIPs at the default app icon size.
pub const DEFAULT_RESERVED_HEIGHT: f32 = logic::HEIGHT + 2.0 * logic::MARGIN;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{
    CoTaskMemFree,
    StructuredStorage::{PropVariantClear, PropVariantToStringAlloc},
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetCapture, GetDoubleClickTime, GetKeyState, ReleaseCapture, SetCapture, TME_LEAVE,
    TRACKMOUSEEVENT, TrackMouseEvent, VK_ESCAPE, VK_LBUTTON, VK_RBUTTON, VK_SHIFT,
};
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, Error, GUID, Result, w};

use crate::commands::open_windows::{self, Window};
use crate::config::{Backdrop, DockAnimation, WindowListMode};
use crate::indexer::{AppEntry, IconPixels};
use crate::theme::Palette;
use crate::tray_host::{self, Action, TrayHost};

pub const WM_SHOW_LAUNCHER: u32 = WM_APP + 20;
const WM_REFRESH: u32 = WM_APP + 21;
const WM_DOCK_TRAY_SYNC: u32 = WM_APP + 32;
/// The tray host's icons changed.
const WM_TRAY_CHANGED: u32 = WM_APP + 22;
/// Asks the owner to show the Windows taskbar for a moment, for icons the
/// dock lacks.
pub const WM_DOCK_SHOW_TASKBAR: u32 = WM_APP + 23;
const WM_CLOSE_FLYOUT: u32 = WM_APP + 24;
const WM_SURFACE_MATERIAL: u32 = WM_APP + 50;
#[path = "dock/sound.rs"]
pub mod sound;
/// Shows a notification icon's tooltip after a short hover.
const TIP_TIMER: usize = 104;
const TIP_MS: u32 = 450;
/// Tooltips wrap past this width, in DIPs.
const TIP_MAX: f32 = 320.0;
/// Lists an app's windows after the same hover, and hides it once the pointer
/// has left both the list and the dock.
const WINDOWS_TIMER: usize = 107;
const WINDOWS_HIDE_TIMER: usize = 108;
const PREVIEW_ZOOM_TIMER: usize = 111;
pub(crate) const POPUP_FADE_TIMER: usize = 110;
#[derive(Clone, Copy)]
pub(crate) struct PopupFade {
    started: Instant,
    ms: u32,
    motion: crate::animation::Motion,
}
/// Remote tray frames reuse their complete bitmap for opacity-only ticks.
type RemotePopupFade = PopupFade;
impl PopupFade {
    const INTERVAL: u32 = crate::animation::REMOTE_INTERVAL;
    fn from_motion(mut motion: crate::animation::Motion, opening: bool, ms: u32) -> Self {
        motion.retarget_duration(opening, ms);
        Self {
            started: Instant::now(),
            ms,
            motion,
        }
    }
    fn new(ms: u32, enabled: bool) -> Option<Self> {
        (ms != 0 && enabled).then(|| Self::from_motion(crate::animation::Motion::at(190. / 255.), true, ms.min(120)))
    }
    fn sample(self) -> crate::animation::Motion {
        let mut motion = self.motion;
        motion.tick(self.started.elapsed().as_secs_f32());
        motion
    }
    fn alpha_at(elapsed: Duration, ms: u32) -> u8 {
        let mut motion = crate::animation::Motion::at(190. / 255.);
        motion.retarget_duration(true, ms);
        motion.tick(elapsed.as_secs_f32());
        (motion.value * 255.).round() as u8
    }
    fn alpha(self) -> u8 {
        (self.sample().value * 255.).round().clamp(0., 255.) as u8
    }
    fn finished(self) -> bool {
        self.started.elapsed().as_millis() >= u128::from(self.ms) || !self.sample().active()
    }
}
#[cfg(test)]
mod remote_popup_tests {
    use super::*;
    #[test]
    fn tray_close_reopens_from_its_actual_position_and_velocity() {
        let mut opening = PopupFade::from_motion(crate::animation::Motion::at(0.), true, 180);
        opening.started -= Duration::from_millis(60);
        let from = opening.sample();
        let closing = PopupFade::from_motion(from, false, 140);
        assert_eq!(
            (closing.motion.value, closing.motion.velocity),
            (from.value, from.velocity)
        );
        let reopening = PopupFade::from_motion(closing.motion, true, 180);
        assert_eq!(
            (reopening.motion.value, reopening.motion.velocity),
            (from.value, from.velocity)
        );
        let mut retired = closing;
        retired.started -= Duration::from_secs(1);
        assert!(retired.finished());
        assert_eq!(retired.alpha(), 0);
    }
    #[test]
    fn remote_popup_fade_is_bounded_and_respects_disabled_motion() {
        assert!(RemotePopupFade::new(0, true).is_none());
        assert!(RemotePopupFade::new(120, false).is_none());
        assert_eq!(RemotePopupFade::new(500, true).unwrap().ms, 120);
        assert_eq!(RemotePopupFade::new(35, true).unwrap().ms, 35);
        let alphas = [0, 30, 60, 90, 120, 300].map(|ms| RemotePopupFade::alpha_at(Duration::from_millis(ms), 120));
        assert_eq!(alphas[0], 190);
        assert_eq!(alphas[4], 255);
        assert_eq!(alphas[5], 255);
        assert!(alphas.windows(2).all(|pair| pair[0] <= pair[1]));
    }
    #[test]
    fn remote_popup_reopen_starts_visible_and_cancel_retires_the_old_fade() {
        let mut fade = RemotePopupFade::new(120, true);
        let old = fade.as_mut().unwrap();
        old.started = Instant::now() - Duration::from_millis(300);
        assert!(old.finished());
        assert_eq!(old.alpha(), 255);
        fade.take(); // Close/menu completion retires the active fade, even mid-open.
        assert!(fade.is_none());
        fade = RemotePopupFade::new(120, true);
        assert!(!fade.unwrap().finished());
        assert!(fade.unwrap().alpha() >= 190);
    }
}
const POPUP_START_ALPHA: u8 = 115;

pub(crate) fn client_animations_enabled() -> bool {
    let mut enabled = BOOL(1);
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&raw mut enabled).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    enabled.as_bool()
}

pub(crate) fn start_popup_fade(hwnd: HWND, ms: u32) -> Option<PopupFade> {
    if ms == 0 || !client_animations_enabled() {
        return None;
    }
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style | WS_EX_LAYERED.0 as isize);
        if SetLayeredWindowAttributes(hwnd, COLORREF(0), POPUP_START_ALPHA, LWA_ALPHA).is_err()
            || SetTimer(Some(hwnd), POPUP_FADE_TIMER, crate::animation::LOCAL_INTERVAL, None) == 0
        {
            finish_popup_fade(hwnd);
            return None;
        }
    }
    Some(PopupFade::from_motion(
        crate::animation::Motion::at(POPUP_START_ALPHA as f32 / 255.),
        true,
        ms,
    ))
}

pub(crate) fn tick_popup_fade(hwnd: HWND, fade: &mut Option<PopupFade>) {
    let Some(animation) = *fade else { return };
    if animation.finished() {
        fade.take();
        finish_popup_fade(hwnd);
    } else {
        unsafe {
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), animation.alpha(), LWA_ALPHA);
        }
    }
}

pub(crate) fn finish_popup_fade(hwnd: HWND) {
    unsafe {
        let _ = KillTimer(Some(hwnd), POPUP_FADE_TIMER);
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style & !(WS_EX_LAYERED.0 as isize));
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}
const WINDOWS_HIDE_MS: u32 = 400;
const REFRESH_TIMER: usize = 101;
/// Watches the pointer for the bottom edge while the dock is hidden, and
/// for leaving while it shows.
const POLL_TIMER: usize = 102;
const POLL_MS: u32 = 100;
/// Polls the pointer may spend away from the dock before it hides.
const HIDE_AFTER: u8 = 3;
/// Steps the slide-in animation.
const SLIDE_TIMER: usize = 103;
const HOVER_TIMER: usize = 105;
const HOVER_MS: f32 = 110.0;
/// Hides the desktop pill after a scroll.
const DESKTOP_TIMER: usize = 106;
const DESKTOP_PILL_MS: u32 = 1100;
/// Wheel input right after a switch is the same flick, not another request.
const DESKTOP_PAUSE: Duration = Duration::from_millis(300);
thread_local! {
    static SURFACE_CACHE: RefCell<Option<(Instant, Rc<Vec<SurfaceWindow>>)>> = const { RefCell::new(None) };
    static DOCK_WINDOWS: RefCell<Vec<HWND>> = const { RefCell::new(Vec::new()) };
}

fn post_to_docks(message: u32, except: Option<HWND>) {
    DOCK_WINDOWS.with_borrow(|docks| {
        for &hwnd in docks {
            if Some(hwnd) != except {
                let _ = unsafe { PostMessageW(Some(hwnd), message, WPARAM(0), LPARAM(0)) };
            }
        }
    });
}

#[derive(Default)]
struct DockData {
    pins: RefCell<pins::Pins>,
    tray: RefCell<Option<TrayHost>>,
    tray_anchor: Cell<HWND>,
    topbar_status: Cell<bool>,
    topbar_strips: RefCell<Option<topbar_space::Strips>>,
    dock_enabled: Cell<bool>,
    windows: RefCell<Option<(Instant, Rc<Vec<Window>>)>>,
}

impl DockData {
    fn new() -> Self {
        Self {
            pins: RefCell::new(pins::Pins::load()),
            dock_enabled: Cell::new(true),
            ..Default::default()
        }
    }

    fn windows(&self) -> Rc<Vec<Window>> {
        let mut cache = self.windows.borrow_mut();
        if let Some((at, windows)) = &*cache
            && at.elapsed() < Duration::from_millis(75)
        {
            return windows.clone();
        }
        let windows = Rc::new(open_windows::list());
        *cache = Some((Instant::now(), windows.clone()));
        windows
    }
}

fn surface_windows() -> Rc<Vec<SurfaceWindow>> {
    SURFACE_CACHE.with_borrow_mut(|cache| {
        if let Some((at, windows)) = &*cache
            && at.elapsed() < Duration::from_millis(150)
        {
            return windows.clone();
        }
        let mut windows = Vec::new();
        let _ = unsafe { EnumWindows(Some(collect_surface), LPARAM(&raw mut windows as isize)) };
        let windows = Rc::new(windows);
        *cache = Some((Instant::now(), windows.clone()));
        windows
    })
}

pub(crate) fn primary_fullscreen() -> bool {
    monitor_info(primary_monitor()).is_some_and(|info| fullscreen_on(&surface_windows(), info.rcMonitor, info.rcWork))
}

struct Item {
    app: AppEntry,
    windows: Vec<Window>,
}

enum DockMenu {
    App {
        hwnd: HWND,
        app: AppEntry,
        pinned: bool,
        windows: Vec<(isize, u32)>,
    },
    Chevron {
        hwnd: HWND,
        owner: HWND,
    },
}
enum DockEffect {
    None,
    Controls,
    Launch { app: AppEntry, indexed: bool },
    Close(Vec<(isize, u32)>),
    Taskbar(HWND),
    Open(&'static str),
    Focus((isize, u32)),
    Minimize(Vec<(isize, u32)>),
}

/// Native modal dispatch is allowed to pump app updates. Only snapshot and
/// result application borrow State; the native callback owns no RefCell guard.
fn modal_action<S, T, R>(
    cell: &RefCell<S>,
    prepare: impl FnOnce(&mut S) -> Option<T>,
    display: impl FnOnce(&T) -> u32,
    apply: impl FnOnce(&mut S, T, u32) -> R,
) -> Option<R> {
    let snapshot = { prepare(&mut cell.borrow_mut()) }?;
    let command = display(&snapshot);
    Some(apply(&mut cell.borrow_mut(), snapshot, command))
}

#[derive(Clone, Copy)]
struct PanelTarget {
    id: sound::Identity,
    epoch: u64,
    material_version: u64,
}
fn panel_generation_current(expected: (u64, u64), current: (u64, u64), same_panel: bool, owner_current: bool) -> bool {
    owner_current && same_panel && expected == current
}
fn dock_panel_current(cell: &RefCell<State>, target: PanelTarget) -> bool {
    let current = {
        let s = cell.borrow();
        panel_generation_current(
            (target.epoch, target.material_version),
            (s.sound_epoch, s.material_version),
            s.sound_identity == Some(target.id),
            s.alive && unsafe { GetWindowLongPtrW(s.hwnd, GWLP_USERDATA) == cell as *const _ as isize },
        )
    };
    current && sound::is_current(target.id)
}

struct AppPress {
    id: String,
    start: POINT,
    pointer: Option<LPARAM>,
    dragging: bool,
}

struct State {
    alive: bool,
    hwnd: HWND,
    owner: HWND,
    apps: Vec<AppEntry>,
    icons: HashMap<String, Arc<IconPixels>>,
    items: Vec<Item>,
    data: Rc<DockData>,
    primary: bool,
    app_press: Option<AppPress>,
    palette: Palette,
    tip_palette: Palette,
    windows_palette: Palette,
    zoom_palette: Palette,
    backdrop: Backdrop,
    backdrop_color: String,
    material_options: crate::theme::MaterialOptions,
    dark: bool,
    window_list_mode: WindowListMode,
    app_icon_size: f32,
    renderer: render::Renderer,
    dpi: u32,
    hover: Option<usize>,
    visible: bool,
    fullscreen: bool,
    scheduled: bool,
    scroll: usize,
    capacity: usize,
    status: status::Status,
    status_read_at: Instant,
    hover_part: Option<logic::Part>,
    /// The pointer touched the bottom edge, so the dock slid in.
    revealed: bool,
    away: u8,
    /// Stays revealed regardless of the pointer, for the demo's screenshots,
    /// optionally at another DPI than the monitor's.
    held: bool,
    /// Whether the dock hides until the pointer touches a bottom edge. When
    /// false it stays on the primary screen.
    autohide: bool,
    reveal_height: u32,
    smart_hide: bool,
    remote: bool,
    current_monitor_only: bool,
    preview_current_monitor_only: bool,
    monitor: HMONITOR,
    fullscreen_clear_since: Option<Instant>,
    suspended: bool,
    frame_paced: bool,
    held_dpi: Option<u32>,
    /// Where the window rests, in physical pixels, and when it began sliding there.
    top: i32,
    slide: Option<Instant>,
    slide_motion: crate::animation::Motion,
    slide_to: f32,
    slide_ms: u32,
    open_ms: u32,
    close_ms: u32,
    animation: DockAnimation,
    layered: bool,
    tray_interaction: Option<(u32, Instant)>,
    sound: HWND,
    sound_identity: Option<sound::Identity>,
    sound_epoch: u64,
    sound_state: crate::commands::State,
    sound_hardware: crate::commands::control::Hardware,
    previews: Vec<Option<isize>>,
    preview_height: f32,
    preview_width: f32,
    preview_page: usize,
    preview_count: usize,
    windows_clicked: bool,
    native_menu_open: bool,
    hover_levels: Vec<f32>,
    hover_tick: Option<Instant>,
    /// Wheel delta over the volume not yet spent on a whole percent.
    wheel: i32,
    /// Wheel travel toward the next desktop switch, and when the last one happened.
    desktop_travel: i32,
    desktop_switched: Option<Instant>,
    /// The desktops the pill above the dock shows, while it shows.
    desktop_pill: Option<desktops::Desktops>,
    /// The dock's left edge on screen, in physical pixels.
    left: i32,
    width: i32,
    surface_rect: Option<RECT>,
    /// Hosts the notification area while the taskbar is hidden.
    tray: Vec<tray::Shown>,
    /// Indices into `tray` of the icons shown in the dock itself.
    inline: Vec<usize>,
    /// Lists every notification icon above the chevron.
    flyout: HWND,
    flyout_open: bool,
    start_open: bool,
    flyout_fade: Option<PopupFade>,
    remote_flyout_fade: Option<RemotePopupFade>,
    flyout_exit: Option<PopupFade>,
    flyout_material: Option<crate::theme::SurfaceMaterial>,
    flyout_palette: Palette,
    material_version: u64,
    flyout_prepared: bool,
    flyout_hover: Option<usize>,
    flyout_renderer: render::Renderer,
    tip: HWND,
    tip_renderer: render::Renderer,
    /// What the pointer rests on, and the text the tooltip shows for it.
    tip_target: Option<tray::Tip>,
    tip_text: String,
    /// The window list above the hovered app icon: which slot it belongs to,
    /// its rows and their windows, and the row under the pointer.
    windows: HWND,
    windows_renderer: render::Renderer,
    windows_slot: Option<usize>,
    windows_rows: Vec<String>,
    windows_monitors: Vec<String>,
    windows_targets: Vec<isize>,
    /// Full chooser order, including off-page cards, independent of app z-order.
    windows_order: Vec<isize>,
    windows_hover: Option<(usize, bool)>,
    windows_press: Option<logic::PreviewPress>,
    zoom: HWND,
    zoom_renderer: render::Renderer,
    zoom_thumbnail: Option<isize>,
    zoom_source: Option<(isize, u32)>,
    zoom_title: String,
    /// The last click on a notification icon, to tell double clicks.
    tray_click: Option<(tray_host::Key, Instant)>,
    /// The demo's flyout and its hovered icon, applied as icons arrive.
    held_flyout: Option<Option<usize>>,
}

/// Own on the launcher's message-loop thread. Never borrows Launcher from its wndproc.
pub struct Dock {
    hwnd: HWND,
    shared: bool,
    state: Rc<RefCell<State>>,
    hooks: Vec<HWINEVENTHOOK>,
    secondary: Vec<Dock>,
    topbars: Vec<topbar::Bar>,
    topbar_space: Option<topbar_space::TopbarSpace>,
}

/// Owned pin handle for Start: persistence and dock notifications run after
/// releasing the Launcher borrow, just like native app actions.
#[derive(Clone)]
pub(crate) struct AppPins(Rc<DockData>);
impl AppPins {
    pub fn contains(&self, id: &str) -> bool {
        self.0.pins.borrow().position(id).is_some()
    }
    pub fn set(&self, app: AppEntry, pinned: bool) -> std::io::Result<()> {
        self.0.pins.borrow_mut().set_pinned(app, pinned)?;
        post_to_docks(WM_REFRESH, None);
        Ok(())
    }
}

pub(crate) fn set_start_pin(handle: Option<&AppPins>, app: AppEntry, pinned: bool) -> std::io::Result<()> {
    if let Some(handle) = handle {
        handle.set(app, pinned)
    } else {
        pins::Pins::load().set_pinned(app, pinned)
    }
}

pub(crate) fn saved_start_pinned(id: &str) -> bool {
    pins::Pins::load().position(id).is_some()
}

impl Dock {
    pub(crate) fn app_pins(&self) -> AppPins {
        AppPins(self.state.borrow().data.clone())
    }
    pub fn new(owner: HWND) -> Result<Self> {
        Self::create(owner, false, primary_monitor(), Rc::new(DockData::new()), true)
    }

    pub fn on_surface(owner: HWND) -> Result<Self> {
        Self::create(owner, true, primary_monitor(), Rc::new(DockData::new()), true)
    }

    fn create(owner: HWND, shared: bool, monitor: HMONITOR, data: Rc<DockData>, primary: bool) -> Result<Self> {
        let state = Rc::new(RefCell::new(State {
            alive: true,
            hwnd: HWND::default(),
            owner,
            apps: Vec::new(),
            icons: HashMap::new(),
            items: Vec::new(),
            data,
            primary,
            app_press: None,
            palette: Palette::new(true, true),
            tip_palette: Palette::new(true, false),
            windows_palette: Palette::new(true, false),
            zoom_palette: Palette::new(true, false),
            backdrop: Backdrop::Acrylic,
            backdrop_color: String::new(),
            material_options: Default::default(),
            dark: true,
            window_list_mode: WindowListMode::default(),
            app_icon_size: logic::DEFAULT_APP_ICON_SIZE,
            renderer: render::Renderer::new()?,
            dpi: 96,
            hover: None,
            visible: false,
            fullscreen: false,
            scheduled: false,
            scroll: 0,
            capacity: usize::MAX,
            status: status::read(),
            status_read_at: Instant::now(),
            hover_part: None,
            revealed: false,
            away: 0,
            held: false,
            autohide: true,
            reveal_height: 8,
            smart_hide: false,
            remote: false,
            current_monitor_only: false,
            preview_current_monitor_only: false,
            monitor,
            fullscreen_clear_since: None,
            suspended: false,
            frame_paced: false,
            held_dpi: None,
            top: 0,
            slide: None,
            slide_motion: crate::animation::Motion::at(1.),
            slide_to: 0.0,
            slide_ms: 0,
            open_ms: 180,
            close_ms: 140,
            animation: DockAnimation::Slide,
            layered: false,
            tray_interaction: None,
            sound: HWND::default(),
            sound_identity: None,
            sound_epoch: 0,
            sound_state: crate::commands::State::default(),
            sound_hardware: crate::commands::control::Hardware::default(),
            previews: Vec::new(),
            preview_height: 0.,
            preview_width: 0.,
            preview_page: 0,
            preview_count: 0,
            windows_clicked: false,
            native_menu_open: false,
            hover_levels: Vec::new(),
            hover_tick: None,
            wheel: 0,
            desktop_travel: 0,
            desktop_switched: None,
            desktop_pill: None,
            left: 0,
            width: 0,
            surface_rect: None,
            tray: Vec::new(),
            inline: Vec::new(),
            flyout: HWND::default(),
            flyout_open: false,
            start_open: false,
            flyout_fade: None,
            remote_flyout_fade: None,
            flyout_exit: None,
            flyout_material: None,
            flyout_palette: Palette::new(true, false),
            material_version: 0,
            flyout_prepared: false,
            flyout_hover: None,
            flyout_renderer: render::Renderer::new()?,
            tip: HWND::default(),
            tip_renderer: render::Renderer::new()?,
            tip_target: None,
            tip_text: String::new(),
            windows: HWND::default(),
            windows_renderer: render::Renderer::new()?,
            windows_slot: None,
            windows_rows: Vec::new(),
            windows_monitors: Vec::new(),
            windows_targets: Vec::new(),
            windows_order: Vec::new(),
            windows_hover: None,
            windows_press: None,
            zoom: HWND::default(),
            zoom_renderer: render::Renderer::new()?,
            zoom_thumbnail: None,
            zoom_source: None,
            zoom_title: String::new(),
            tray_click: None,
            held_flyout: None,
        }));
        let instance = unsafe { GetModuleHandleW(None)? };
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("Wisp.Dock"),
            hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? },
            ..Default::default()
        };
        if unsafe { RegisterClassExW(&class) } == 0 && unsafe { GetLastError() } != ERROR_CLASS_ALREADY_EXISTS {
            return Err(Error::from_thread());
        }
        let hwnd = if shared {
            unsafe {
                SetWindowLongPtrW(owner, GWLP_USERDATA, Rc::as_ptr(&state) as isize);
            }
            owner
        } else {
            unsafe {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                    class.lpszClassName,
                    w!("Wisp Dock"),
                    WS_POPUP | WS_CAPTION,
                    0,
                    0,
                    64,
                    44,
                    None,
                    None,
                    Some(instance.into()),
                    Some(Rc::as_ptr(&state).cast()),
                )?
            }
        };
        state.borrow_mut().hwnd = hwnd;
        let mut dock = Self {
            hwnd,
            shared,
            state,
            hooks: Vec::new(),
            secondary: Vec::new(),
            topbars: Vec::new(),
            topbar_space: None,
        };
        // The flyout and tooltips are owned by the dock, so they stay above it.
        let popup = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(popup_wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("Wisp.DockPopup"),
            hCursor: class.hCursor,
            ..Default::default()
        };
        if unsafe { RegisterClassExW(&popup) } == 0 && unsafe { GetLastError() } != ERROR_CLASS_ALREADY_EXISTS {
            return Err(Error::from_thread());
        }
        for (style, title) in [
            // The flyout takes focus, for Esc and closing on a click elsewhere.
            (WS_EX_TOOLWINDOW | WS_EX_TOPMOST, w!("Notification area")),
            (
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
                w!("Wisp Dock tooltip"),
            ),
            // The window list takes clicks, but not the focus.
            (
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                w!("Wisp Dock windows"),
            ),
            (
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_LAYERED,
                w!("Wisp Window preview"),
            ),
        ] {
            let window = unsafe {
                CreateWindowExW(
                    style,
                    popup.lpszClassName,
                    title,
                    WS_POPUP | WS_CAPTION,
                    0,
                    0,
                    1,
                    1,
                    Some(hwnd),
                    None,
                    Some(instance.into()),
                    Some(Rc::as_ptr(&dock.state).cast()),
                )?
            };
            let mut s = dock.state.borrow_mut();
            if s.flyout.is_invalid() {
                s.flyout = window;
            } else if s.tip.is_invalid() {
                s.tip = window;
            } else if s.windows.is_invalid() {
                s.windows = window;
            } else {
                s.zoom = window;
            }
        }
        DOCK_WINDOWS.with_borrow_mut(|docks| docks.push(hwnd));
        if primary {
            for (first, last) in [
                (EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE),
                (EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_NAMECHANGE),
                (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE),
                (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
                // Windows on other desktops are cloaked, so switching desktops changes the apps.
                (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
            ] {
                let hook = unsafe {
                    SetWinEventHook(
                        first,
                        last,
                        None,
                        Some(window_event),
                        0,
                        0,
                        WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                    )
                };
                if hook.is_invalid() {
                    return Err(Error::from_thread());
                }
                dock.hooks.push(hook);
            }
        }
        if primary {
            dock.state.borrow().data.tray_anchor.set(hwnd);
        }
        dock.apply_theme(true, Backdrop::Acrylic, "", Default::default());
        if !primary {
            dock.state.borrow_mut().sync_tray();
        }
        dock.refresh();
        Ok(dock)
    }

    /// Keeps one independent dock on each attached monitor.
    pub fn sync_monitors(&mut self) -> Result<()> {
        if self.shared {
            return Ok(());
        }
        let previous: Vec<_> = std::iter::once(&*self)
            .chain(&self.secondary)
            .map(|dock| dock.state.borrow().monitor)
            .collect();
        let main = primary_monitor();
        let mut monitors = Vec::new();
        unsafe { EnumDisplayMonitors(None, None, Some(collect_monitor), LPARAM(&raw mut monitors as isize)).ok()? };
        let (owner, data, reveal_height) = {
            let mut state = self.state.borrow_mut();
            state.set_monitor(main);
            (state.owner, state.data.clone(), state.reveal_height)
        };
        self.secondary.retain(|dock| {
            let monitor = dock.state.borrow().monitor;
            monitor != main && monitors.contains(&monitor)
        });
        for monitor in monitors.into_iter().filter(|&monitor| monitor != main) {
            if !self.secondary.iter().any(|dock| dock.state.borrow().monitor == monitor) {
                let mut dock = Self::create(owner, false, monitor, data.clone(), false)?;
                dock.set_reveal_height(reveal_height);
                self.secondary.push(dock);
            }
        }
        let current: Vec<_> = std::iter::once(&*self)
            .chain(&self.secondary)
            .map(|dock| dock.state.borrow().monitor)
            .collect();
        if previous != current {
            crate::logging::event("dock.monitors.changed", format_args!("count={}", current.len()));
        }
        let mut state = self.state.borrow_mut();
        if data.tray_anchor.get().is_invalid() {
            data.tray_anchor.set(self.hwnd);
        }
        state.refresh();
        Ok(())
    }

    fn for_monitor(&self, monitor: HMONITOR) -> &Self {
        self.secondary
            .iter()
            .find(|dock| dock.state.borrow().monitor == monitor)
            .unwrap_or(self)
    }

    pub fn start_rect(&self) -> RECT {
        std::iter::once(self)
            .chain(&self.secondary)
            .find(|dock| dock.state.borrow().start_open)
            .unwrap_or(self)
            .resting_rect()
    }

    pub fn draw_on(&mut self, rt: &windows::Win32::Graphics::Direct2D::ID2D1HwndRenderTarget) -> Result<()> {
        self.state.borrow_mut().paint(Some(rt))
    }

    pub fn shared(&self) -> bool {
        self.shared
    }

    pub fn dispatch(&self, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe { wndproc(self.hwnd, msg, wp, lp) }
    }

    pub fn owns_message(msg: u32, wp: WPARAM) -> bool {
        msg == WM_TIMER && (REFRESH_TIMER..=WINDOWS_HIDE_TIMER).contains(&wp.0)
            || matches!(msg, WM_REFRESH | WM_TRAY_CHANGED | WM_SURFACE_MATERIAL)
    }

    pub fn mostly_visible(&self) -> bool {
        let s = self.state.borrow();
        unsafe { crate::morph::can_morph(IsWindowVisible(self.hwnd).as_bool(), s.slide_value()) }
    }

    pub fn pace_frames(&mut self) {
        self.state.borrow_mut().frame_paced = true;
    }

    /// Slides the dock in as the pointer would, and keeps it in, for captures.
    pub fn capture_reveal(&mut self) {
        let mut s = self.state.borrow_mut();
        s.held = true;
        s.reveal();
    }

    pub fn capture_slide(&mut self) {
        let mut s = self.state.borrow_mut();
        let to = if s.slide_to > 0.5 { 0.0 } else { 1.0 };
        s.revealed = to < 0.5;
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
        s.start_slide(to);
    }

    pub fn animating(&self) -> bool {
        self.state.borrow().slide.is_some()
    }

    pub fn frame(&mut self) {
        let mut s = self.state.borrow_mut();
        if s.slide.is_some() {
            s.step_slide();
        }
    }

    /// Pauses placement without hiding the shared surface.
    pub fn suspend(&mut self) -> RECT {
        let mut s = self.state.borrow_mut();
        s.cancel_app_press();
        s.close_flyout();
        s.close_sound();
        s.hide_tip();
        s.hide_windows();
        let mut rect = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut rect);
            if !IsWindowVisible(self.hwnd).as_bool() {
                let height = rect.bottom - rect.top;
                let layout_scale = logic::layout_scale(s.app_icon_size);
                rect.top =
                    s.top + height + (logic::margin(layout_scale, s.remote) * s.dpi as f32 / 96.0).round() as i32;
                rect.bottom = rect.top + height;
            }
            let _ = KillTimer(Some(self.hwnd), POLL_TIMER);
        }
        s.stop_slide();
        s.suspended = true;
        rect
    }

    pub fn resume(&mut self) {
        let mut s = self.state.borrow_mut();
        s.suspended = false;
        s.surface_rect = None;
        s.revealed = true;
        s.visible = true;
        s.away = 0;
        s.stop_slide();
        s.refresh();
        unsafe {
            SetTimer(Some(self.hwnd), POLL_TIMER, POLL_MS, None);
        }
    }

    pub fn surface_rect(&mut self, rect: RECT) {
        let mut s = self.state.borrow_mut();
        s.surface_rect = Some(rect);
        s.publish_rects();
    }

    pub fn resting_rect(&self) -> RECT {
        let s = self.state.borrow();
        RECT {
            left: s.left,
            top: s.top,
            right: s.left + s.width,
            bottom: s.top + (logic::height(logic::layout_scale(s.app_icon_size)) * s.dpi as f32 / 96.0).round() as i32,
        }
    }

    /// Call after index or icon batches.
    pub fn set_apps(&mut self, apps: &[AppEntry], icons: &HashMap<String, Arc<IconPixels>>) {
        for dock in &mut self.secondary {
            dock.set_apps(apps, icons);
        }
        let mut s = self.state.borrow_mut();
        s.apps = apps.to_vec();
        for (id, pixels) in icons {
            s.icons.entry(id.clone()).or_insert_with(|| pixels.clone());
        }
        s.refresh();
    }

    pub fn set_window_list_mode(&mut self, mode: WindowListMode) {
        for dock in &mut self.secondary {
            dock.set_window_list_mode(mode);
        }
        let mut s = self.state.borrow_mut();
        s.window_list_mode = mode;
        s.hide_windows();
    }

    pub fn set_app_icon_size(&mut self, size: u32) {
        for dock in &mut self.secondary {
            dock.set_app_icon_size(size);
        }
        let size = (size as f32).clamp(logic::MIN_APP_ICON_SIZE, logic::MAX_APP_ICON_SIZE);
        let mut s = self.state.borrow_mut();
        if s.app_icon_size == size {
            return;
        }
        s.app_icon_size = size;
        s.place();
        unsafe {
            let _ = InvalidateRect(Some(s.hwnd), None, false);
        }
    }

    pub fn set_remote(&mut self, remote: bool) {
        for dock in &mut self.secondary {
            dock.set_remote(remote);
        }
        if self.state.borrow().remote == remote {
            return;
        }
        let material = self.state.borrow_mut().flyout_material.take();
        if let Some(host) = &material {
            host.hide();
        }
        self.state.borrow_mut().flyout_material = material;
        let mut s = self.state.borrow_mut();
        if s.remote == remote {
            return;
        }
        s.close_flyout();
        if s.remote {
            // ULW and native alpha must never share an uncleared layered style.
            unsafe {
                let style = GetWindowLongPtrW(s.flyout, GWL_EXSTYLE);
                SetWindowLongPtrW(s.flyout, GWL_EXSTYLE, style & !(WS_EX_LAYERED.0 as isize));
            }
            s.flyout_renderer.discard_popup_surface();
        }
        s.remote = remote;
        s.material_version = s.material_version.wrapping_add(1);
        let _ = unsafe { PostMessageW(Some(s.hwnd), WM_SURFACE_MATERIAL, WPARAM(0), LPARAM(0)) };
        if remote {
            s.flyout_palette = crate::theme::apply_layered_surface(
                s.flyout,
                s.dark,
                s.backdrop,
                &s.backdrop_color,
                s.material_options,
            );
        } else {
            s.flyout_palette = crate::theme::apply_backdrop(
                s.flyout,
                s.dark,
                s.backdrop,
                true,
                &s.backdrop_color,
                s.material_options,
            );
        }
        s.hover_part = None;
        s.close_sound();
        s.hide_tip();
        s.place();
        unsafe {
            let _ = InvalidateRect(Some(s.hwnd), None, false);
        }
    }

    pub fn set_animation(&mut self, open_ms: u32, close_ms: u32, animation: DockAnimation) {
        for dock in &mut self.secondary {
            dock.set_animation(open_ms, close_ms, animation);
        }
        let mut s = self.state.borrow_mut();
        s.open_ms = open_ms;
        s.close_ms = close_ms;
        s.animation = animation;
    }

    pub fn sound_window(&self, id: usize) -> Option<HWND> {
        self.topbars.iter().find_map(|bar| bar.panel(id)).or_else(|| {
            std::iter::once(self).chain(&self.secondary).find_map(|dock| {
                let state = dock.state.borrow();
                (state.sound.0 as usize == id && state.sound_open()).then_some(state.sound)
            })
        })
    }

    pub fn update_control_hardware(&self, hardware: crate::commands::control::Hardware) {
        for bar in &self.topbars {
            bar.update_hardware(hardware.clone());
        }
        for dock in &self.secondary {
            dock.update_control_hardware(hardware.clone());
        }
        let panel_hardware = hardware.clone();
        unborrowed_action(
            &self.state,
            |s| {
                s.sound_hardware = hardware;
                s.sound_identity.map(|id| PanelTarget {
                    id,
                    epoch: s.sound_epoch,
                    material_version: s.material_version,
                })
            },
            |target| {
                if let Some(target) = target.filter(|target| dock_panel_current(&self.state, *target)) {
                    sound::update_hardware(target.id.hwnd, panel_hardware);
                }
            },
        );
    }

    pub fn confirm_wifi(&self, on: bool) {
        for bar in &self.topbars {
            bar.confirm_wifi(on);
        }
        for dock in &self.secondary {
            dock.confirm_wifi(on);
        }
        let target = {
            let s = self.state.borrow();
            s.sound_identity.map(|id| PanelTarget {
                id,
                epoch: s.sound_epoch,
                material_version: s.material_version,
            })
        };
        if let Some(target) = target.filter(|target| dock_panel_current(&self.state, *target)) {
            sound::confirm_wifi(target.id.hwnd, on);
        }
    }
    pub fn update_sound(&self, state: &crate::commands::State) {
        for bar in &self.topbars {
            bar.update_commands(state);
        }
        for dock in &self.secondary {
            dock.update_sound(state);
        }
        unborrowed_action(
            &self.state,
            |s| {
                s.sound_state.clone_from(state);
                s.sound_identity.map(|id| PanelTarget {
                    id,
                    epoch: s.sound_epoch,
                    material_version: s.material_version,
                })
            },
            |target| {
                if let Some(target) = target.filter(|target| dock_panel_current(&self.state, *target)) {
                    sound::update(target.id.hwnd, state);
                    if dock_panel_current(&self.state, target) {
                        let hardware = self.state.borrow().sound_hardware.clone();
                        sound::update_hardware(target.id.hwnd, hardware);
                    }
                }
            },
        );
    }

    pub fn refresh(&mut self) {
        self.state.borrow_mut().refresh();
    }

    pub fn set_topbar(&mut self, config: &crate::config::Config) -> Result<()> {
        self.state.borrow().data.dock_enabled.set(config.dock);
        let options = topbar::Options::from_config(config);
        let displays = topbar::displays()
            .into_iter()
            .filter(|d| config.topbar && topbar::model::selected(&options.monitors, &d.id, d.primary))
            .collect::<Vec<_>>();
        self.topbars
            .retain(|bar| displays.iter().any(|d| d.handle == bar.monitor()));
        let (owner, data) = {
            let s = self.state.borrow();
            (s.owner, s.data.clone())
        };
        let mut failure = None;
        for display in &displays {
            if !self.topbars.iter().any(|bar| bar.monitor() == display.handle) {
                match topbar::Bar::new(owner, display, data.clone(), options.clone()) {
                    Ok(bar) => self.topbars.push(bar),
                    Err(error) => failure = Some(error),
                }
            }
        }
        for (i, bar) in self.topbars.iter().enumerate() {
            bar.configure(options.clone(), i == 0);
        }
        let targets = displays
            .iter()
            .filter(|d| self.topbars.iter().any(|bar| bar.monitor() == d.handle))
            .map(|d| (d.id.clone(), options.height as f32))
            .collect::<Vec<_>>();
        if targets.is_empty() {
            let material = self.state.borrow_mut().flyout_material.take();
            drop(material);
            self.topbar_space = None;
        } else if let Some(space) = &self.topbar_space {
            space.set_targets(targets);
        } else {
            match topbar_space::TopbarSpace::new(targets) {
                Ok(space) => self.topbar_space = Some(space),
                Err(error) => failure = Some(error),
            }
        }
        *data.topbar_strips.borrow_mut() = self.topbar_space.as_ref().map(|space| space.strips());
        data.topbar_status.set(!self.topbars.is_empty());
        data.tray_anchor
            .set(self.topbars.first().map_or(self.hwnd, |bar| HWND(bar.id() as _)));
        for dock in std::iter::once(&*self).chain(&self.secondary) {
            let mut s = dock.state.borrow_mut();
            s.close_flyout();
            s.close_sound();
            s.place();
            let _ = unsafe { InvalidateRect(Some(s.hwnd), None, false) };
        }
        if let Some(error) = failure { Err(error) } else { Ok(()) }
    }

    pub fn set_media_exclusions(&mut self, exclusions: &[String]) {
        for bar in &self.topbars {
            bar.set_media_exclusions(exclusions);
        }
    }
    pub fn topbar_active(&self) -> bool {
        !self.topbars.is_empty()
    }
    pub fn topbar_matches(&self, id: usize) -> bool {
        self.topbars.iter().any(|bar| bar.id() == id)
    }
    pub fn topbar_polling(&self, id: usize) -> bool {
        self.topbar_matches(id) && self.topbars.iter().any(|bar| bar.visible())
    }
    pub fn update_topbar_status(
        &self,
        audio: Option<crate::commands::audio::State>,
        wifi: Option<crate::commands::wifi::State>,
    ) {
        for bar in &self.topbars {
            bar.update_status(audio.clone(), wifi.clone());
        }
    }
    pub fn update_topbar(&self, id: usize, track: Option<crate::commands::media::NotchTrack>) {
        if self.topbar_matches(id) {
            for bar in &self.topbars {
                bar.update_track(track.clone());
            }
        }
    }
    pub fn take_topbar_action(&self, id: usize) -> Option<crate::commands::Command> {
        self.topbars.iter().find(|bar| bar.id() == id)?.take_action()
    }

    pub fn apply_theme(&mut self, dark: bool, backdrop: Backdrop, color: &str, options: crate::theme::MaterialOptions) {
        let material = self.state.borrow_mut().flyout_material.take();
        drop(material);
        for dock in &mut self.secondary {
            dock.apply_theme(dark, backdrop, color, options);
        }
        self.state.borrow_mut().apply_material(dark, backdrop, color, options);
    }

    /// Hosts the notification area in the dock while `enabled`, which
    /// should follow the hidden taskbar. Failure leaves icons to Explorer.
    pub fn set_tray(&mut self, enabled: bool) {
        let mut s = self.state.borrow_mut();
        let data = s.data.clone();
        if enabled == data.tray.borrow().is_some() {
            return;
        }
        if enabled {
            match TrayHost::start(self.hwnd, WM_TRAY_CHANGED) {
                Ok(host) => *data.tray.borrow_mut() = Some(host),
                Err(error) => {
                    eprintln!("Dock notification area: {error}");
                    return;
                }
            }
        } else {
            s.close_flyout();
            s.hide_tip();
            *data.tray.borrow_mut() = None;
        }
        s.sync_tray();
        post_to_docks(WM_DOCK_TRAY_SYNC, Some(self.hwnd));
    }

    /// Keeps notification icons reaching Wisp after Explorer shows its taskbar.
    pub fn outrank_tray(&self) {
        if let Some(host) = &*self.state.borrow().data.tray.borrow() {
            host.outrank();
        }
    }

    /// Lets the dock auto-hide: it appears when the pointer reaches the
    /// bottom edge of a screen.
    pub fn set_start_open(&mut self, open: bool) {
        let mut cursor = POINT::default();
        let _ = unsafe { GetCursorPos(&mut cursor) };
        let monitor = unsafe { MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST) };
        let selected = self.for_monitor(monitor).hwnd;
        for dock in std::iter::once(&*self).chain(&self.secondary) {
            let should_open = open && dock.hwnd == selected;
            if dock.state.borrow().start_open != should_open {
                dock.set_start_on_dock(should_open);
            }
        }
    }

    fn set_start_on_dock(&self, open: bool) {
        let mut s = self.state.borrow_mut();
        s.start_open = open;
        s.away = 0;
        if open {
            s.close_flyout();
            s.close_sound();
            s.hide_tip();
            s.hide_windows();
            if !s.visible {
                s.visible = true;
                unsafe {
                    let _ = SetTimer(Some(s.hwnd), POLL_TIMER, POLL_MS, None);
                }
            }
            if !s.revealed && !s.sample_fullscreen() {
                s.reveal();
            }
        } else if s.autohide
            && !s.smart_clear()
            && s.revealed
            && !s.held
            && !s.flyout_open
            && !s.sound_open()
            && !s.tray_menu_active()
        {
            let slack = (12 * s.dpi / 96) as i32;
            let mut cursor = POINT::default();
            if unsafe { GetCursorPos(&mut cursor) }.is_ok()
                && !s.over_bottom_strip(cursor)
                && (cursor.x < s.left - slack || cursor.x >= s.left + s.width + slack || cursor.y < s.top - slack)
            {
                s.hide_tip();
                s.revealed = false;
                s.start_slide(1.);
            }
        }
    }

    pub fn show(&mut self) {
        if !self.state.borrow().data.dock_enabled.get() {
            return;
        }
        for dock in &mut self.secondary {
            dock.show();
        }
        self.state.borrow_mut().suspended = false;
        let mut s = self.state.borrow_mut();
        s.visible = true;
        // A dock that stays visible comes back in place, without sliding.
        s.revealed = !s.autohide;
        s.surface_rect = None;
        s.stop_slide();
        unsafe {
            SetTimer(Some(self.hwnd), POLL_TIMER, POLL_MS, None);
        }
        s.refresh();
    }
    /// Reveals the dock for good, with a fixed hover, so screenshots can show it
    /// without moving the pointer.
    pub fn hold(&mut self, hover: Option<usize>, hover_part: Option<&str>, dpi: Option<u32>) {
        let mut s = self.state.borrow_mut();
        s.held = true;
        s.held_dpi = dpi;
        s.revealed = true;
        s.hover = hover;
        s.animate_hover();
        s.hover_part = match hover_part {
            Some("volume") => Some(logic::Part::Volume),
            Some("battery") => Some(logic::Part::Battery),
            Some("clock") => Some(logic::Part::Clock),
            Some("chevron") => Some(logic::Part::Chevron),
            Some(part) => part
                .strip_prefix("tray")
                .and_then(|i| i.parse().ok())
                .map(logic::Part::Tray),
            _ => None,
        };
        // "flyout" opens the flyout, "flyout2" also hovers its third icon.
        s.held_flyout = hover_part
            .and_then(|part| part.strip_prefix("flyout"))
            .map(|rest| rest.parse().ok());
        s.reveal();
        s.stop_slide();
        s.place();
        if hover_part == Some("desktops") {
            s.show_desktops(desktops::read());
        }
        s.sync_tray();
    }
    /// Keeps the dock visible on the primary screen, or lets it auto-hide.
    pub fn set_visibility(&mut self, autohide: bool, smart_hide: bool) {
        for dock in &mut self.secondary {
            dock.set_visibility(autohide, smart_hide);
        }
        let mut s = self.state.borrow_mut();
        let smart_hide = autohide && smart_hide;
        if (s.autohide, s.smart_hide) == (autohide, smart_hide) {
            return;
        }
        s.autohide = autohide;
        s.smart_hide = smart_hide;
        if !autohide && s.visible && !s.suspended && !s.revealed {
            s.reveal();
        } else {
            // Move to the primary screen, or start the hide countdown.
            s.away = 0;
            if s.revealed {
                s.refresh();
            } else {
                s.place();
            }
        }
    }

    pub fn set_current_monitor_only(&mut self, enabled: bool) {
        for dock in &mut self.secondary {
            dock.set_current_monitor_only(enabled);
        }
        let mut s = self.state.borrow_mut();
        if s.current_monitor_only != enabled {
            s.current_monitor_only = enabled;
            s.refresh();
        }
    }

    pub fn set_reveal_height(&mut self, physical_pixels: u32) {
        for dock in &mut self.secondary {
            dock.set_reveal_height(physical_pixels);
        }
        let mut state = self.state.borrow_mut();
        let height = physical_pixels.clamp(1, 128);
        if state.reveal_height != height {
            state.reveal_height = height;
            state.away = 0;
        }
    }

    pub fn set_preview_current_monitor_only(&mut self, enabled: bool) {
        for dock in &mut self.secondary {
            dock.set_preview_current_monitor_only(enabled);
        }
        let mut s = self.state.borrow_mut();
        if s.preview_current_monitor_only != enabled {
            s.preview_current_monitor_only = enabled;
            s.hide_windows();
        }
    }

    /// Whether the dock stays on screen instead of auto-hiding.
    pub fn stays_visible(&self) -> bool {
        !self.state.borrow().autohide
    }

    /// The room a dock that stays visible keeps at the bottom of the primary
    /// screen, in DIPs: its height plus external margins outside Remote mode.
    pub fn reserved_height(&self) -> f32 {
        let s = self.state.borrow();
        let scale = logic::layout_scale(s.app_icon_size);
        logic::reserved_height(scale, s.remote)
    }

    /// Hides the dock until `show`, such as while the launcher is open.
    pub fn hide(&mut self) {
        for dock in &mut self.secondary {
            dock.hide();
        }
        let mut s = self.state.borrow_mut();
        s.cancel_app_press();
        s.close_flyout();
        s.close_sound();
        s.hide_tip();
        s.hide_windows();
        s.visible = false;
        s.stop_slide();
        s.revealed = false;
        unsafe {
            let _ = KillTimer(Some(self.hwnd), POLL_TIMER);
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }
}

impl Drop for Dock {
    fn drop(&mut self) {
        self.topbar_space = None;
        self.topbars.clear();
        self.secondary.clear();
        DOCK_WINDOWS.with_borrow_mut(|docks| docks.retain(|&hwnd| hwnd != self.hwnd));
        {
            let state = self.state.borrow();
            if state.data.tray_anchor.get() == self.hwnd {
                state.data.tray_anchor.set(HWND::default());
            }
        }
        unsafe {
            for hook in self.hooks.drain(..) {
                let _ = UnhookWinEvent(hook);
            }
            let _ = KillTimer(Some(self.hwnd), REFRESH_TIMER);
            let _ = KillTimer(Some(self.hwnd), POLL_TIMER);
            let _ = KillTimer(Some(self.hwnd), SLIDE_TIMER);
            let _ = KillTimer(Some(self.hwnd), TIP_TIMER);
            let _ = KillTimer(Some(self.hwnd), HOVER_TIMER);
            let _ = KillTimer(Some(self.hwnd), DESKTOP_TIMER);
            let _ = KillTimer(Some(self.hwnd), WINDOWS_TIMER);
            let _ = KillTimer(Some(self.hwnd), WINDOWS_HIDE_TIMER);
            let _ = KillTimer(Some(self.hwnd), PREVIEW_ZOOM_TIMER);
            // Remove the pointer before synchronous destruction can reenter the wndproc.
            let mut s = self.state.borrow_mut();
            s.alive = false;
            s.close_sound();
            s.clear_preview();
            s.hide_zoom();
            for hwnd in [s.flyout, s.tip, s.windows, s.zoom, self.hwnd] {
                if !hwnd.is_invalid() {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
            }
            drop(s);
            // Destroys the owned flyout, tooltip and window list too.
            if self.shared {
                let _ = DestroyWindow(self.state.borrow().flyout);
                let _ = DestroyWindow(self.state.borrow().tip);
                let _ = DestroyWindow(self.state.borrow().windows);
                let _ = DestroyWindow(self.state.borrow().zoom);
            } else {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

impl State {
    fn paint(&mut self, rt: Option<&windows::Win32::Graphics::Direct2D::ID2D1HwndRenderTarget>) -> Result<()> {
        let hwnd = self.hwnd;
        let parts = self.parts();
        let animation_scale = self.animation_scale();
        let sound_open = self.sound_open();
        let drag = self.drag_frame();
        let State {
            renderer,
            items,
            icons,
            palette,
            dpi,
            hover,
            hover_levels,
            app_icon_size,
            capacity,
            scroll,
            status,
            hover_part,
            tray,
            inline,
            flyout_open,
            ..
        } = self;
        let frame = &render::Frame {
            animation_scale,
            dpi: *dpi,
            palette,
            items,
            icons,
            hover: *hover,
            hover_levels,
            app_icon_size: *app_icon_size,
            layout_scale: logic::layout_scale(*app_icon_size),
            capacity: *capacity,
            scroll: *scroll,
            status,
            hover_part: *hover_part,
            parts: &parts,
            tray,
            inline,
            flyout_open: *flyout_open,
            sound_open,
            drag,
        };
        match rt {
            Some(rt) => renderer.draw_on(rt, frame),
            None => renderer.draw(hwnd, frame),
        }
    }

    fn apply_material(&mut self, dark: bool, backdrop: Backdrop, color: &str, options: crate::theme::MaterialOptions) {
        self.material_version = self.material_version.wrapping_add(1);
        self.flyout_prepared = false;
        let _ = unsafe { PostMessageW(Some(self.hwnd), WM_SURFACE_MATERIAL, WPARAM(0), LPARAM(0)) };
        self.material_options = options;
        self.backdrop = backdrop;
        self.backdrop_color = color.to_owned();
        self.dark = dark;
        self.palette = crate::theme::apply_backdrop(self.hwnd, dark, backdrop, true, color, options);
        for (hwnd, palette) in [
            (self.flyout, &mut self.flyout_palette),
            (self.tip, &mut self.tip_palette),
            (self.windows, &mut self.windows_palette),
            (self.zoom, &mut self.zoom_palette),
        ] {
            *palette = if hwnd == self.flyout && self.remote {
                crate::theme::apply_layered_surface(hwnd, dark, backdrop, color, options)
            } else {
                crate::theme::apply_backdrop(hwnd, dark, backdrop, true, color, options)
            };
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
        }
    }

    fn refresh(&mut self) {
        if self.suspended || !self.data.dock_enabled.get() {
            return;
        }
        // Pointer auto-hide can catch up on reveal. Smart mode also uses this
        // path while the focused app is maximized on this display.
        if self.visible && !self.revealed && self.slide.is_none() && !self.held && !self.smart_clear() {
            unsafe {
                if IsWindowVisible(self.hwnd).as_bool() {
                    let _ = ShowWindow(self.hwnd, SW_HIDE);
                }
            }
            return;
        }
        let windows = self.data.windows();
        let mut items: Vec<Item> = Vec::new();
        for window in windows.iter().cloned() {
            let aumid = window_aumid(HWND(window.hwnd as _));
            let app = logic::match_app(&self.apps, &window.exe, aumid.as_deref())
                .cloned()
                .unwrap_or_else(|| {
                    let id = aumid.filter(|s| !s.is_empty()).unwrap_or_else(|| window.exe.clone());
                    AppEntry {
                        desktop: None,
                        name: open_windows::program_name(&window.exe).into(),
                        id,
                        path: (!window.exe.is_empty()).then(|| window.exe.clone()),
                    }
                });
            if app.id.is_empty() {
                continue;
            }
            if let Some(item) = items.iter_mut().find(|item| item.app.id.eq_ignore_ascii_case(&app.id)) {
                item.windows.push(window);
            } else {
                items.push(Item {
                    app,
                    windows: vec![window],
                });
            }
        }
        let data = self.data.clone();
        let pins = data.pins.borrow();
        let mut pinned = Vec::with_capacity(pins.apps.len());
        for app in &pins.apps {
            let item = if let Some(at) = items.iter().position(|item| item.app.id.eq_ignore_ascii_case(&app.id)) {
                items.remove(at)
            } else {
                Item {
                    app: self
                        .apps
                        .iter()
                        .find(|indexed| indexed.id.eq_ignore_ascii_case(&app.id))
                        .unwrap_or(app)
                        .clone(),
                    windows: Vec::new(),
                }
            };
            pinned.push(item);
        }
        drop(pins);
        if self.current_monitor_only {
            items.retain(|item| {
                item.windows
                    .iter()
                    .any(|window| monitor_for_window(window.hwnd) == self.monitor)
            });
        }
        // A slot may now belong to another app, or contain different windows.
        let previous = self
            .windows_slot
            .and_then(|slot| self.item_at(slot))
            .map(|item| item.app.id.clone());
        pinned.extend(logic::stable_order(
            std::mem::take(&mut self.items),
            items,
            |old, new| old.app.id.eq_ignore_ascii_case(&new.app.id),
        ));
        self.items = pinned;
        let mut previews_changed = false;
        if let Some(slot) = self.windows_slot {
            let same_app = self.item_at(slot).zip(previous.as_ref()).filter(|(item, id)| {
                let count = self.preview_windows(item).len();
                id.eq_ignore_ascii_case(&item.app.id)
                    && count > 0
                    && (self.windows_clicked || logic::show_window_list(self.window_list_mode, count))
            });
            if let Some((item, _)) = same_app {
                let candidates = self.preview_windows(item);
                previews_changed = !self.windows_rows.is_empty()
                    && (self.windows_order.len() != candidates.len()
                        || self
                            .windows_order
                            .iter()
                            .any(|old| !candidates.iter().any(|new| *old == new.hwnd)));
            } else {
                self.hide_windows();
            }
        }
        self.status = status::read();
        self.place();
        if self.suspended {
            return;
        }
        self.sample_fullscreen();
        if !self.fullscreen {
            if previews_changed && self.windows_slot.is_some() {
                self.show_windows();
            } else {
                self.refresh_window_titles();
            }
        }
        if self.fullscreen {
            self.hide_for_fullscreen();
        }
        unsafe {
            let _ = ShowWindow(
                self.hwnd,
                if self.visible && (self.revealed || self.slide.is_some()) && !self.fullscreen {
                    SW_SHOWNOACTIVATE
                } else {
                    SW_HIDE
                },
            );
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    fn count(&self) -> usize {
        self.items.len().saturating_sub(self.scroll).min(self.capacity)
    }

    fn tray_menu_active(&self) -> bool {
        self.tray_interaction.is_some_and(|(thread, started)| {
            let mut info = GUITHREADINFO {
                cbSize: size_of::<GUITHREADINFO>() as u32,
                ..Default::default()
            };
            started.elapsed() < Duration::from_millis(500)
                || unsafe { GetGUIThreadInfo(thread, &mut info) }.is_ok()
                    && (info.flags.0 & (GUI_INMENUMODE.0 | GUI_POPUPMENUMODE.0) != 0 || !info.hwndCapture.is_invalid())
        })
    }

    fn poll(&mut self) {
        if !self.data.dock_enabled.get() || !self.visible || self.suspended {
            return;
        }
        let _span = crate::util::perf::Span::new("dock_poll");
        if let Some((source, expected_pid)) = self.zoom_source {
            let mut pid = 0;
            unsafe { GetWindowThreadProcessId(HWND(source as _), Some(&mut pid)) };
            if pid != expected_pid || !unsafe { IsWindowVisible(HWND(source as _)) }.as_bool() {
                self.hide_zoom();
            }
        }
        let mut cursor = POINT::default();
        if unsafe { GetCursorPos(&mut cursor) }.is_err() {
            return;
        }
        let tray_busy = self.tray_menu_active();
        if !tray_busy && self.tray_interaction.take().is_some() {
            self.restore_tray_order();
        }
        let focused = focused_surface();
        if self.sample_fullscreen_for(focused.as_ref()) {
            if self.revealed || self.slide.is_some() {
                self.hide_for_fullscreen();
            }
            return;
        }
        // Actual fullscreen keeps its hard suppression above. Maximized apps
        // instead use pointer auto-hide, including the normal bottom reveal.
        let smart_clear = self.smart_hide && !focused_maximized_on(focused.as_ref(), self.monitor);
        let (in_reveal_strip, in_bottom_strip) = self.bottom_strips(cursor);
        if !self.revealed {
            let mouse_held =
                unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0 || GetAsyncKeyState(VK_RBUTTON.0 as i32) < 0 };
            let reveal_from_strip = self.autohide && logic::reveal_from_hover_strip(in_reveal_strip, mouse_held);
            if !self.autohide || smart_clear || reveal_from_strip {
                self.reveal();
            }
            return;
        }
        let mut rect = RECT::default();
        let _ = unsafe { GetWindowRect(self.hwnd, &mut rect) };
        // The dock does not take focus. A release outside a background window
        // can miss its capture handler, so cancel that gesture on the existing poll.
        if self.app_press.is_some()
            && (!(rect.left..rect.right).contains(&cursor.x) || !(rect.top..rect.bottom).contains(&cursor.y))
            && unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) >= 0 }
        {
            self.cancel_app_press();
        }
        if self.flyout_open && !self.held && !tray_busy {
            // Activation closes the flyout on a click elsewhere; this covers
            // clicks while it could not take the foreground.
            let pressed =
                unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0 || GetAsyncKeyState(VK_RBUTTON.0 as i32) < 0 };
            let mut flyout = RECT::default();
            let _ = unsafe { GetWindowRect(self.flyout, &mut flyout) };
            let inside =
                |r: &RECT| cursor.x >= r.left && cursor.x < r.right && cursor.y >= r.top && cursor.y < r.bottom;
            if pressed && !inside(&flyout) && !inside(&rect) {
                self.dismiss_flyout();
            }
        }
        let slack = (12 * self.dpi / 96) as i32;
        // The gap below the dock counts as on it, so the pointer can rest on the edge.
        // The dock stays while its flyout is open.
        let over_windows = self.over_preview(cursor);
        let interacting = self.start_open
            || self.native_menu_open
            || tray_busy
            || self.sound_open()
            || self.held
            || self.flyout_open
            || self.app_press.is_some()
            || over_windows;
        let pointer_near = cursor.x >= rect.left - slack
            && cursor.x < rect.right + slack
            && cursor.y >= rect.top - slack
            && cursor.y < rect.bottom + slack;
        let near = hover_keeps_dock_visible(!self.autohide, smart_clear, in_bottom_strip, interacting, pointer_near);
        self.away = logic::hover_away_polls(self.away, near);
        if self.away >= HIDE_AFTER {
            self.hide_tip();
            self.hide_windows();
            self.revealed = false;
            self.start_slide(1.0);
            return;
        }
        // Polling also catches volume changes from keys and other apps.
        if self.status_read_at.elapsed() >= Duration::from_millis(750) {
            self.update_status();
        }
    }

    fn over_bottom_strip(&self, cursor: POINT) -> bool {
        self.bottom_strips(cursor).1
    }

    fn bottom_strips(&self, cursor: POINT) -> (bool, bool) {
        monitor_info(self.monitor).map_or((false, false), |info| {
            let monitor = info.rcMonitor;
            let bounds = (monitor.left, monitor.top, monitor.right, monitor.bottom);
            logic::bottom_hover_zones(
                (cursor.x, cursor.y),
                bounds,
                self.dpi,
                self.app_icon_size,
                self.reveal_height,
                self.remote,
            )
        })
    }

    /// Shows the dock, or reverses an outgoing slide at its current position.
    fn reveal(&mut self) {
        let hidden = !self.revealed && self.slide.is_none();
        self.revealed = true;
        self.away = 0;
        if hidden {
            self.slide_to = 1.0;
        }
        self.start_slide(0.0);
        self.refresh();
    }

    fn sampled_slide(&self) -> crate::animation::Motion {
        let mut motion = self.slide_motion;
        if let Some(start) = self.slide {
            motion.tick(start.elapsed().as_secs_f32() / crate::util::perf::slowdown());
        } else {
            motion = crate::animation::Motion::at(1. - self.slide_to);
        }
        motion
    }
    fn slide_value(&self) -> f32 {
        1. - self.sampled_slide().value
    }

    fn start_slide(&mut self, to: f32) {
        if to > 0.5 {
            self.hide_windows();
        }
        crate::util::perf::event("slide_begin");
        self.slide_motion = self.sampled_slide();
        self.slide_to = to;
        self.slide_ms = if to > 0.5 { self.close_ms } else { self.open_ms };
        let animate = self.slide_ms > 0 && client_animations_enabled();
        self.slide_motion
            .retarget_duration(to < 0.5, if animate { self.slide_ms } else { 0 });
        self.slide = self.slide_motion.active().then(Instant::now);
        if self.slide.is_none() {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), SLIDE_TIMER);
                if !self.revealed {
                    let _ = ShowWindow(self.hwnd, SW_HIDE);
                }
            }
            self.set_slide_alpha(255);
            return;
        }
        self.apply_slide();
        if !self.frame_paced {
            unsafe {
                if SetTimer(
                    Some(self.hwnd),
                    SLIDE_TIMER,
                    crate::animation::interval(self.remote),
                    None,
                ) == 0
                {
                    self.slide_motion.settle();
                    self.slide = None;
                    self.apply_slide();
                    if !self.revealed {
                        let _ = ShowWindow(self.hwnd, SW_HIDE);
                    }
                }
            }
        }
    }

    fn stop_slide(&mut self) {
        self.slide = None;
        self.slide_motion = crate::animation::Motion::at(1.);
        self.slide_to = 0.0;
        self.set_slide_alpha(255);
        unsafe {
            let _ = KillTimer(Some(self.hwnd), SLIDE_TIMER);
        }
    }

    fn slide_offset(&self, height: i32) -> i32 {
        let distance = match self.animation {
            DockAnimation::Slide => {
                height
                    + (logic::margin(logic::layout_scale(self.app_icon_size), self.remote) * self.dpi as f32 / 96.0)
                        .round() as i32
            }
            DockAnimation::RiseFade => (12.0 * self.dpi as f32 / 96.0).round() as i32,
            DockAnimation::Fade | DockAnimation::ScaleFade => 0,
        };
        (distance as f32 * self.slide_value()).round() as i32
    }

    fn animation_scale(&self) -> f32 {
        if self.animation == DockAnimation::ScaleFade && self.slide.is_some() {
            1.0 - 0.08 * self.slide_value()
        } else {
            1.0
        }
    }

    fn set_slide_alpha(&mut self, alpha: u8) {
        let layered = alpha < 255;
        unsafe {
            if self.layered != layered {
                let style = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
                let flag = WS_EX_LAYERED.0 as isize;
                SetWindowLongPtrW(
                    self.hwnd,
                    GWL_EXSTYLE,
                    if layered { style | flag } else { style & !flag },
                );
                self.layered = layered;
                if !layered {
                    let _ = InvalidateRect(Some(self.hwnd), None, false);
                }
            }
            if self.layered {
                let _ = SetLayeredWindowAttributes(self.hwnd, COLORREF(0), alpha, LWA_ALPHA);
            }
        }
    }

    fn apply_slide(&mut self) {
        let height = (logic::height(logic::layout_scale(self.app_icon_size)) * self.dpi as f32 / 96.0).round() as i32;
        let factor = self.animation_scale();
        let width = (self.width as f32 * factor).round() as i32;
        let scaled_height = (height as f32 * factor).round() as i32;
        let x = self.left + (self.width - width) / 2;
        let y = self.top + self.slide_offset(height) + height - scaled_height;
        unsafe {
            let mut rect = RECT::default();
            if width > 0
                && height > 0
                && GetWindowRect(self.hwnd, &mut rect).is_ok()
                && (rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top) != (x, y, width, scaled_height)
            {
                let _ = SetWindowPos(
                    self.hwnd,
                    None,
                    x,
                    y,
                    width,
                    scaled_height,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                self.publish_rects();
            }
        }
        let alpha = if self.animation == DockAnimation::Slide {
            255
        } else {
            ((1.0 - self.slide_value()) * 255.0).round() as u8
        };
        self.set_slide_alpha(alpha);
    }

    fn step_slide(&mut self) {
        let _span = crate::util::perf::Span::new("slide");
        crate::util::perf::event("slide_frame");
        let tick = Instant::now();
        let elapsed = self.slide.map_or(0.0, |s| s.elapsed().as_secs_f64() * 1000.0);
        let sampled = self.sampled_slide();
        let finished = !sampled.active();
        if finished {
            self.slide = None;
        }
        self.apply_slide();
        if finished {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), SLIDE_TIMER);
                if !self.revealed {
                    let _ = ShowWindow(self.hwnd, SW_HIDE);
                    self.set_slide_alpha(255);
                }
            }
        }
        if crate::util::perf::hook_on("WISP_FRAME_TIMING") {
            std::eprintln!(
                "slide frame timestamp_ms={elapsed:.3} work_ms={:.3}",
                tick.elapsed().as_secs_f64() * 1000.0
            );
        }
    }

    fn animate_hover(&mut self) {
        if self.hover_tick.is_none() {
            self.hover_tick = Some(Instant::now());
        }
        unsafe {
            SetTimer(Some(self.hwnd), HOVER_TIMER, 10, None);
        }
    }

    fn step_hover(&mut self) {
        let now = Instant::now();
        let step = self.hover_tick.replace(now).map_or(1.0, |last| {
            now.duration_since(last).as_secs_f32() * 1000.0 / crate::util::perf::slowdown() / HOVER_MS
        });
        self.hover_levels.resize(self.items.len() + 1, 0.0);
        let mut moving = false;
        for (index, level) in self.hover_levels.iter_mut().enumerate() {
            let goal = if self.hover == Some(index) { 1.0 } else { 0.0 };
            *level += (goal - *level).clamp(-step, step);
            moving |= *level != goal;
        }
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
            if !moving {
                self.hover_tick = None;
                let _ = KillTimer(Some(self.hwnd), HOVER_TIMER);
            }
        }
    }

    fn dock_monitor(&self) -> HMONITOR {
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        unsafe {
            if !self.monitor.is_invalid() && GetMonitorInfoW(self.monitor, &mut info).as_bool() {
                self.monitor
            } else {
                primary_monitor()
            }
        }
    }

    fn set_monitor(&mut self, monitor: HMONITOR) {
        if self.monitor != monitor {
            self.monitor = monitor;
            self.fullscreen = false;
            self.fullscreen_clear_since = None;
            self.hide_windows();
            self.close_sound();
            self.close_flyout();
            self.place();
        }
    }

    fn sample_fullscreen(&mut self) -> bool {
        let focused = focused_surface();
        self.sample_fullscreen_for(focused.as_ref())
    }

    fn sample_fullscreen_for(&mut self, focused: Option<&SurfaceWindow>) -> bool {
        let fullscreen =
            monitor_info(self.monitor).is_some_and(|info| focused_fullscreen_on(focused, info.rcMonitor, info.rcWork));
        if fullscreen {
            self.fullscreen = true;
            self.fullscreen_clear_since = None;
        } else if self.fullscreen {
            let clear_since = self.fullscreen_clear_since.get_or_insert_with(Instant::now);
            if clear_since.elapsed() >= Duration::from_millis(350) {
                self.fullscreen = false;
                self.fullscreen_clear_since = None;
            }
        }
        self.fullscreen
    }

    fn smart_clear(&self) -> bool {
        self.smart_hide && !focused_maximized_on(focused_surface().as_ref(), self.monitor)
    }

    fn move_tray_anchor_after_hide(&self) {
        if self.data.tray_anchor.get() != self.hwnd {
            return;
        }
        let visible = DOCK_WINDOWS.with_borrow(|docks| {
            docks
                .iter()
                .copied()
                .find(|&hwnd| hwnd != self.hwnd && unsafe { IsWindowVisible(hwnd) }.as_bool())
        });
        if let Some(hwnd) = visible {
            self.data.tray_anchor.set(hwnd);
            let _ = unsafe { PostMessageW(Some(hwnd), WM_DOCK_TRAY_SYNC, WPARAM(0), LPARAM(0)) };
        }
    }

    fn hide_for_fullscreen(&mut self) {
        self.cancel_app_press();
        self.stop_slide();
        self.revealed = false;
        self.close_sound();
        self.close_flyout();
        self.hide_tip();
        self.hide_windows();
        let _ = unsafe { ShowWindow(self.hwnd, SW_HIDE) };
        self.move_tray_anchor_after_hide();
    }

    fn place(&mut self) {
        let previous = (self.dpi, self.left, self.top, self.width, self.scroll, self.capacity);
        if self.suspended {
            return;
        }
        unsafe {
            let monitor = self.dock_monitor();
            self.monitor = monitor;
            let mut info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(monitor, &mut info).as_bool() {
                return;
            }
            // Modal menu loops keep delivering periodic layout/status updates.
            let order = dock_menu_order(self.native_menu_open, self.tray_interaction.is_some());
            // Move first, then query the window's new monitor DPI. Skipped on the
            // same monitor, where the jump would show.
            if MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONULL) != monitor {
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    info.rcMonitor.left,
                    info.rcMonitor.bottom - 60,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOREDRAW | order,
                );
            }
            self.dpi = self.held_dpi.unwrap_or_else(|| GetDpiForWindow(self.hwnd)).max(96);
            let scale = self.dpi as f32 / 96.0;
            // Notification icons are rendered for the DPI they are shown at.
            let size = tray::icon_size(self.dpi);
            if self
                .tray
                .iter()
                .any(|shown| shown.pixels.as_ref().is_some_and(|p| p.width != size as u32))
            {
                let icons = std::mem::take(&mut self.tray)
                    .into_iter()
                    .map(|shown| shown.icon)
                    .collect();
                (self.tray, self.inline) = tray::update(Vec::new(), icons, size);
            }
            let size = logic::icon_pixels(self.dpi, self.app_icon_size);
            for item in &self.items {
                if self.icons.get(&item.app.id).is_none_or(|p| p.width != size as u32) {
                    let path = item
                        .app
                        .path
                        .clone()
                        .unwrap_or_else(|| format!("shell:AppsFolder\\{}", item.app.id));
                    if let Some(pixels) = tray::app_pixels(&path, size) {
                        self.icons.insert(item.app.id.clone(), Arc::new(pixels));
                    }
                }
            }
            let parts = self.parts();
            let layout_scale = logic::layout_scale(self.app_icon_size);
            let available = (info.rcMonitor.right - info.rcMonitor.left) as f32 / scale
                - 8.0 * layout_scale
                - logic::status_width_scaled(&parts, layout_scale);
            self.capacity = (((available - 2.0 * logic::pad(layout_scale) - logic::separator_width(layout_scale))
                / logic::slot(layout_scale)) as usize)
                .saturating_sub(1);
            self.scroll = self.scroll.min(self.items.len().saturating_sub(self.capacity));
            let running = self.count();
            let width = (logic::total_width_scaled(running, &parts, layout_scale) * scale).ceil() as i32;
            let height = (logic::height(layout_scale) * scale).round() as i32;
            let x = info.rcMonitor.left + (info.rcMonitor.right - info.rcMonitor.left - width) / 2;
            self.top = logic::resting_top(info.rcMonitor.bottom, self.dpi, self.app_icon_size, self.remote);
            self.left = x;
            self.width = width;
            let factor = self.animation_scale();
            let scaled_width = (width as f32 * factor).round() as i32;
            let scaled_height = (height as f32 * factor).round() as i32;
            let y = self.top + self.slide_offset(height) + height - scaled_height;
            crate::theme::shape(self.hwnd, width, height, 8.0 * layout_scale, self.dpi);
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                x + (width - scaled_width) / 2,
                y,
                scaled_width,
                scaled_height,
                SWP_NOACTIVATE | order,
            );
        }
        if previous != (self.dpi, self.left, self.top, self.width, self.scroll, self.capacity) {
            if previous.4 == self.scroll
                && !self.windows_rows.is_empty()
                && self.windows_slot.is_some_and(|slot| slot <= self.count())
            {
                self.show_windows();
            } else {
                self.hide_windows();
            }
        }
        if let Some(id) = self.sound_identity {
            if let Some(anchor) = self.part_rect(logic::Part::Volume) {
                sound::update_opener(id, anchor);
            } else {
                self.close_sound();
            }
        }
        // The chevron and icons move whenever the dock's width changes.
        if self.flyout_open {
            self.layout_flyout();
        }
        if unsafe { IsWindowVisible(self.tip) }.as_bool() {
            self.show_tip();
        }
        self.publish_rects();
    }

    /// The status parts, with the notification area while the dock hosts it.
    fn parts(&self) -> Vec<logic::Part> {
        if self.data.topbar_status.get() {
            return Vec::new();
        }
        let parts = self
            .status
            .parts(self.data.tray.borrow().as_ref().map(|_| self.inline.len()));
        if self.remote { logic::remote_parts(parts) } else { parts }
    }

    /// A part's rectangle on screen where the dock rests, in physical pixels.
    fn part_rect(&self, part: logic::Part) -> Option<RECT> {
        let scale = self.dpi as f32 / 96.0;
        let running = self.count();
        let left = logic::part_left_scaled(part, running, &self.parts(), logic::layout_scale(self.app_icon_size))?;
        let px = |dip: f32| (dip * scale).round() as i32;
        let rect = self.surface_rect;
        let x = |dip| match rect {
            Some(rect) => {
                rect.left + (px(dip) as f32 * (rect.right - rect.left) as f32 / self.width.max(1) as f32).round() as i32
            }
            None => self.left + px(dip),
        };
        let layout_scale = logic::layout_scale(self.app_icon_size);
        let height = logic::height(layout_scale);
        let top = rect.map_or(self.top + self.slide_offset(px(height)), |r| r.bottom - px(height));
        Some(RECT {
            left: x(left),
            top,
            right: x(left + part.width_scaled(layout_scale)),
            bottom: top + px(height),
        })
    }

    /// An app slot's rectangle on screen where the dock rests, in physical pixels.
    fn item_rect(&self, slot: usize) -> RECT {
        let scale = self.dpi as f32 / 96.0;
        let px = |dip: f32| (dip * scale).round() as i32;
        let rect = self.surface_rect;
        let x = |dip| match rect {
            Some(rect) => {
                rect.left + (px(dip) as f32 * (rect.right - rect.left) as f32 / self.width.max(1) as f32).round() as i32
            }
            None => self.left + px(dip),
        };
        let layout_scale = logic::layout_scale(self.app_icon_size);
        let height = logic::height(layout_scale);
        let left = logic::left_scaled(slot, layout_scale);
        let top = rect.map_or(self.top + self.slide_offset(px(height)), |r| r.bottom - px(height));
        RECT {
            left: x(left),
            top,
            right: x(left + logic::slot(layout_scale)),
            bottom: top + px(height),
        }
    }

    /// A flyout cell's rectangle on screen, in physical pixels.
    fn cell_rect(&self, index: usize) -> RECT {
        let scale = self.dpi as f32 / 96.0;
        let mut flyout = RECT::default();
        let _ = unsafe { GetWindowRect(self.flyout, &mut flyout) };
        let (left, top) = logic::flyout_cell(index);
        let px = |dip: f32| (dip * scale).round() as i32;
        RECT {
            left: flyout.left + px(left),
            top: flyout.top + px(top),
            right: flyout.left + px(left + logic::FLYOUT_CELL),
            bottom: flyout.top + px(top + logic::FLYOUT_CELL),
        }
    }

    /// Tells the tray host where icons are, for apps that place their
    /// windows by Shell_NotifyIconGetRect. Hidden icons point at the chevron.
    fn publish_rects(&self) {
        if self.data.tray_anchor.get() != self.hwnd {
            return;
        }
        let tray = self.data.tray.borrow();
        let Some(host) = &*tray else { return };
        let mut rects = tray_host::Rects {
            fallback: self.part_rect(logic::Part::Chevron),
            ..Default::default()
        };
        for (position, &index) in self.inline.iter().enumerate() {
            if let Some(rect) = self.part_rect(logic::Part::Tray(position)) {
                rects.icons.insert(self.tray[index].icon.key(), rect);
            }
        }
        if self.flyout_open {
            for (index, shown) in self.tray.iter().enumerate() {
                rects.icons.insert(shown.icon.key(), self.cell_rect(index));
            }
        }
        host.set_rects(rects);
    }

    /// Rereads the tray host's icons.
    fn sync_tray(&mut self) {
        let icons = self
            .data
            .tray
            .borrow()
            .as_ref()
            .map(TrayHost::icons)
            .unwrap_or_default();
        let size = tray::icon_size(self.dpi);
        (self.tray, self.inline) = tray::update(std::mem::take(&mut self.tray), icons, size);
        if self.flyout_hover.is_some_and(|i| i >= self.tray.len()) {
            self.flyout_hover = None;
        }
        let stale = match self.tip_target {
            Some(tray::Tip::Inline(i)) => i >= self.inline.len(),
            Some(tray::Tip::Flyout(i)) => i >= self.tray.len(),
            Some(tray::Tip::Chevron) => self.data.tray.borrow().is_none(),
            // The desktop pill has nothing to do with the tray.
            None => false,
        };
        if stale {
            self.hide_tip();
        }
        if self.tray.is_empty() {
            self.close_flyout();
        }
        self.place();
        if self.held {
            self.apply_hold();
        }
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    /// Shows the demo's held flyout or tooltip once the icons are known.
    fn apply_hold(&mut self) {
        if self.data.tray.borrow().is_none() {
            return;
        }
        if let Some(hover) = self.held_flyout {
            if !self.flyout_open {
                self.open_flyout();
            }
            self.flyout_hover = hover.filter(|&i| i < self.tray.len());
            self.tip_target = self.flyout_hover.map(tray::Tip::Flyout);
        } else {
            self.tip_target = match self.hover_part {
                Some(logic::Part::Chevron) => Some(tray::Tip::Chevron),
                Some(logic::Part::Tray(i)) if i < self.inline.len() => Some(tray::Tip::Inline(i)),
                _ => None,
            };
        }
        self.show_tip();
        unsafe {
            let _ = InvalidateRect(Some(self.flyout), None, false);
        }
    }

    fn toggle_flyout(&mut self) {
        if self.flyout_open && self.flyout_exit.is_none() {
            self.dismiss_flyout();
        } else {
            self.open_flyout();
        }
    }

    fn open_flyout(&mut self) {
        self.data.tray_anchor.set(self.hwnd);
        if self.tray.is_empty() {
            return;
        }
        self.hide_tip();
        self.hide_windows();
        let reverse = self.flyout_exit.take().map(PopupFade::sample);
        self.flyout_open = true;
        self.flyout_prepared = false;
        self.flyout_hover = None;
        self.layout_flyout();
        if self.remote {
            self.remote_flyout_fade = RemotePopupFade::new(self.open_ms, !self.held && client_animations_enabled());
            if let Some(motion) = reverse
                && self.remote_flyout_fade.is_some()
            {
                self.remote_flyout_fade = Some(PopupFade::from_motion(motion, true, self.open_ms.min(120)));
            }
            unsafe {
                let style = GetWindowLongPtrW(self.flyout, GWL_EXSTYLE);
                if style & WS_EX_LAYERED.0 as isize == 0 {
                    SetWindowLongPtrW(self.flyout, GWL_EXSTYLE, style | WS_EX_LAYERED.0 as isize);
                }
            }
            if let Err(error) = self.present_remote_flyout(false) {
                crate::logging::event("tray.remote.prepare.failed", format_args!("error={error}"));
                self.close_flyout();
                return;
            }
            if self.remote_flyout_fade.is_some()
                && unsafe { SetTimer(Some(self.flyout), POPUP_FADE_TIMER, RemotePopupFade::INTERVAL, None) } == 0
            {
                self.remote_flyout_fade = None;
                if let Err(error) = self.present_remote_flyout(true) {
                    crate::logging::event("tray.remote.present.failed", format_args!("error={error}"));
                    self.close_flyout();
                    return;
                }
            }
        } else if !self.held {
            self.flyout_fade = start_popup_fade(self.flyout, self.open_ms);
            if let Some(motion) = reverse
                && self.flyout_fade.is_some()
            {
                let fade = PopupFade::from_motion(motion, true, self.open_ms);
                unsafe {
                    let _ = SetLayeredWindowAttributes(self.flyout, COLORREF(0), fade.alpha(), LWA_ALPHA);
                }
                self.flyout_fade = Some(fade);
            }
        }
        unsafe {
            let _ = ShowWindow(self.flyout, SW_SHOWNA);
            // The click on the chevron lets Wisp take the foreground, which
            // the flyout needs for Esc and to close on a click elsewhere.
            if !self.held {
                let _ = SetForegroundWindow(self.flyout);
            }
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
        self.publish_rects();
    }

    fn present_remote_flyout(&mut self, cached: bool) -> Result<()> {
        self.flyout_prepared = false;
        let alpha = self
            .flyout_exit
            .or(self.remote_flyout_fade)
            .map_or(255, PopupFade::alpha);
        if cached
            && self.flyout_renderer.cached_popup_matches(self.flyout, self.dpi)?
            && !unsafe { GetUpdateRect(self.flyout, None, false) }.as_bool()
        {
            let result = self.flyout_renderer.present_cached_popup(self.flyout, self.dpi, alpha);
            self.flyout_prepared = result.is_ok();
            return result;
        }
        self.flyout_renderer.draw_remote_flyout(
            self.flyout,
            &render::FlyoutFrame {
                dpi: self.dpi,
                palette: &self.flyout_palette,
                tray: &self.tray,
                hover: self.flyout_hover,
            },
            alpha,
        )?;
        // This complete frame includes any pending invalidation on the UI thread.
        unsafe {
            let _ = ValidateRect(Some(self.flyout), None);
        }
        self.flyout_prepared = true;
        Ok(())
    }

    /// Sizes the flyout to its icons and centers it above the chevron.
    fn layout_flyout(&mut self) {
        self.flyout_prepared = false;
        let scale = self.dpi as f32 / 96.0;
        let (width, height) = logic::flyout_size(self.tray.len());
        let (width, height) = ((width * scale).round() as i32, (height * scale).round() as i32);
        let Some(chevron) = self.part_rect(logic::Part::Chevron) else {
            return;
        };
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        unsafe {
            let _ = GetMonitorInfoW(MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST), &mut info);
        }
        let margin = (logic::FLYOUT_GAP * scale).round() as i32;
        let monitor = info.rcMonitor;
        let x = ((chevron.left + chevron.right - width) / 2)
            .min(monitor.right - margin - width)
            .max(monitor.left + margin);
        let y = self.top - margin - height;
        unsafe {
            let _ = SetWindowPos(self.flyout, None, x, y, width, height, SWP_NOACTIVATE | SWP_NOZORDER);
            let _ = InvalidateRect(Some(self.flyout), None, false);
        }
    }

    fn dismiss_flyout(&mut self) {
        if !self.flyout_open || self.flyout_exit.is_some() {
            return;
        }
        let ms = if self.remote {
            self.close_ms.min(100)
        } else {
            self.close_ms
        };
        if ms == 0 || self.held || !client_animations_enabled() {
            self.close_flyout();
            return;
        }
        let motion = self
            .remote_flyout_fade
            .or(self.flyout_fade)
            .map_or(crate::animation::Motion::at(1.), PopupFade::sample);
        let exit = PopupFade::from_motion(motion, false, ms);
        self.flyout_fade = None;
        self.remote_flyout_fade = None;
        self.flyout_exit = Some(exit);
        self.flyout_hover = None;
        self.hide_tip();
        unsafe {
            let style = GetWindowLongPtrW(self.flyout, GWL_EXSTYLE);
            if !self.remote {
                SetWindowLongPtrW(self.flyout, GWL_EXSTYLE, style | WS_EX_LAYERED.0 as isize);
            }
            if SetTimer(
                Some(self.flyout),
                POPUP_FADE_TIMER,
                crate::animation::interval(self.remote),
                None,
            ) == 0
            {
                self.close_flyout();
            }
        }
    }

    fn close_flyout(&mut self) {
        if !self.flyout_open {
            return;
        }
        self.flyout_open = false;
        let was_closing = self.flyout_exit.take().is_some();
        self.flyout_prepared = false;
        self.flyout_hover = None;
        if self.remote {
            self.remote_flyout_fade = None;
            unsafe {
                let _ = KillTimer(Some(self.flyout), POPUP_FADE_TIMER);
            }
        }
        if matches!(self.tip_target, Some(tray::Tip::Flyout(_))) {
            self.hide_tip();
        }
        unsafe {
            let _ = ShowWindow(self.flyout, SW_HIDE);
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
        if self.flyout_fade.take().is_some() || was_closing && !self.remote {
            finish_popup_fade(self.flyout);
        }
        self.publish_rects();
    }

    /// Sends a gesture on `tray[index]` to its app, as Explorer would.
    /// `center` is the icon's center on screen.
    fn click_tray(&mut self, index: usize, action: Action, center: POINT) {
        self.data.tray_anchor.set(self.hwnd);
        self.publish_rects();
        let Some(shown) = self.tray.get(index) else { return };
        let key = shown.icon.key();
        let double = Duration::from_millis(unsafe { GetDoubleClickTime() }.into());
        let action = match action {
            Action::Click
                if self
                    .tray_click
                    .is_some_and(|(last, at)| last == key && at.elapsed() < double) =>
            {
                self.tray_click = None;
                Action::DoubleClick
            }
            Action::Click => {
                self.tray_click = Some((key, Instant::now()));
                Action::Click
            }
            other => other,
        };
        self.hide_tip();
        let menu = matches!(action, Action::Menu);
        if self.flyout_exit.is_some() {
            return;
        }
        if menu && self.remote_flyout_fade.take().is_some() {
            unsafe {
                let _ = KillTimer(Some(self.flyout), POPUP_FADE_TIMER);
            }
            if let Err(error) = self.present_remote_flyout(true) {
                crate::logging::event("tray.remote.present.failed", format_args!("error={error}"));
                self.close_flyout();
            }
        }
        if menu && self.flyout_fade.take().is_some() {
            finish_popup_fade(self.flyout);
        }
        // Keep the overflow visible. Lower the dock and its owned flyout
        // while the app has its menu open so the menu can overlap them.
        // The icon owner needs permission to activate its native menu.
        unsafe {
            let foreground = if self.flyout_open { self.flyout } else { self.hwnd };
            let _ = SetForegroundWindow(foreground);
            if menu {
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_NOTOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
                );
            }
        }
        match tray_host::invoke(&self.tray[index].icon, action, center.x, center.y) {
            Ok(thread) => {
                self.tray_interaction = Some((thread, Instant::now()));
                self.away = 0;
            }
            Err(error) => {
                if menu {
                    self.restore_tray_order();
                }
                eprintln!("Dock notification icon: {error}");
            }
        }
    }

    fn restore_tray_order(&self) {
        if dock_menu_order(self.native_menu_open, self.tray_interaction.is_some()) == SWP_NOZORDER {
            return;
        }
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
            );
        }
    }

    /// An inline icon's gesture; its anchor is the icon's center.
    fn click_inline(&mut self, position: usize, action: Action) {
        let (Some(&index), Some(rect)) = (self.inline.get(position), self.part_rect(logic::Part::Tray(position)))
        else {
            return;
        };
        let center = POINT {
            x: (rect.left + rect.right) / 2,
            y: (rect.top + rect.bottom) / 2,
        };
        self.click_tray(index, action, center);
    }

    /// The pointer rests on `target`; its tooltip follows after a moment.
    fn hover_tip(&mut self, target: Option<tray::Tip>) {
        // The open flyout already shows what the chevron's tooltip offers.
        let target = target.filter(|&t| !(t == tray::Tip::Chevron && self.flyout_open));
        if self.tip_target == target || self.held {
            return;
        }
        self.hide_tip();
        self.tip_target = target;
        if target.is_some() {
            self.hide_windows();
            unsafe {
                SetTimer(Some(self.hwnd), TIP_TIMER, TIP_MS, None);
            }
        }
    }

    /// Lists the windows of the app under the pointer, or of none.
    fn hover_windows(&mut self, slot: Option<usize>) {
        if slot == self.windows_slot && slot.is_some() {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), WINDOWS_HIDE_TIMER);
            }
            return;
        }
        if !self.windows_rows.is_empty() && (slot.is_none() || self.windows_clicked) {
            self.leave_windows();
            return;
        }
        let slot = slot.filter(|&slot| {
            self.window_list_mode != WindowListMode::Off
                && self.item_at(slot).is_some_and(|item| {
                    logic::show_window_list(self.window_list_mode, self.preview_windows(item).len())
                })
        });
        if self.windows_slot == slot {
            return;
        }
        let shown = self.windows_slot.is_some();
        self.hide_windows();
        self.windows_slot = slot;
        if slot.is_some() {
            self.hide_tip();
            unsafe {
                let _ = SetTimer(Some(self.hwnd), WINDOWS_TIMER, if shown { 0 } else { TIP_MS }, None);
            }
        }
    }

    /// The item in a visible slot.
    fn item_at(&self, slot: usize) -> Option<&Item> {
        slot.checked_sub(1).and_then(|i| self.items.get(i + self.scroll))
    }

    fn preview_windows<'a>(&self, item: &'a Item) -> Vec<&'a Window> {
        logic::preview_candidates(&item.windows, self.preview_current_monitor_only, |window| {
            monitor_for_window(window.hwnd) == self.monitor
        })
    }

    /// Hides the window list and forgets its windows.
    fn clear_preview(&mut self) {
        for thumbnail in self.previews.drain(..).flatten() {
            let _ = unsafe { windows::Win32::Graphics::Dwm::DwmUnregisterThumbnail(thumbnail) };
        }
    }

    fn update_previews(&mut self) {
        use windows::Win32::Graphics::Dwm::*;
        let scale = self.dpi as f32 / 96.;
        let px = |dip: f32| (dip * scale).round() as i32;
        self.previews = self
            .windows_targets
            .iter()
            .enumerate()
            .map(|(index, &source)| {
                if self.preview_height < 24. {
                    return None;
                }
                let thumbnail = unsafe { DwmRegisterThumbnail(self.windows, HWND(source as _)) }.ok()?;
                let updated = unsafe {
                    DwmQueryThumbnailSourceSize(thumbnail).is_ok_and(|size| {
                        if size.cx <= 0 || size.cy <= 0 {
                            return false;
                        }
                        let box_width = self.preview_width - 12.;
                        let factor = (box_width / size.cx as f32).min(self.preview_height / size.cy as f32);
                        let width = size.cx as f32 * factor;
                        let height = size.cy as f32 * factor;
                        let left =
                            logic::PREVIEW_PAD + index as f32 * self.preview_width + (self.preview_width - width) / 2.;
                        let top = logic::PREVIEW_PAD + logic::PREVIEW_HEADER + (self.preview_height - height) / 2.;
                        DwmUpdateThumbnailProperties(
                            thumbnail,
                            &DWM_THUMBNAIL_PROPERTIES {
                                dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE | DWM_TNP_OPACITY,
                                rcDestination: RECT {
                                    left: px(left),
                                    top: px(top),
                                    right: px(left + width),
                                    bottom: px(top + height),
                                },
                                opacity: 255,
                                fVisible: BOOL(1),
                                ..Default::default()
                            },
                        )
                        .is_ok()
                    })
                };
                if updated {
                    Some(thumbnail)
                } else {
                    let _ = unsafe { DwmUnregisterThumbnail(thumbnail) };
                    None
                }
            })
            .collect();
    }

    fn hide_windows(&mut self) {
        self.hide_zoom();
        self.windows_press = None;
        if unsafe { GetCapture() } == self.windows {
            let _ = unsafe { ReleaseCapture() };
        }
        self.clear_preview();
        self.windows_slot = None;
        self.windows_clicked = false;
        self.preview_page = 0;
        self.windows_hover = None;
        self.windows_rows.clear();
        self.windows_monitors.clear();
        self.windows_targets.clear();
        self.windows_order.clear();
        unsafe {
            let _ = KillTimer(Some(self.hwnd), WINDOWS_TIMER);
            let _ = KillTimer(Some(self.hwnd), WINDOWS_HIDE_TIMER);
            let _ = ShowWindow(self.windows, SW_HIDE);
        }
    }

    fn preview_press(&self, lp: LPARAM, middle: bool) -> Option<logic::PreviewPress> {
        let (row, close) = self.windows_hit(lp)?;
        let hwnd = *self.windows_targets.get(row)?;
        if self.preview_current_monitor_only && monitor_for_window(hwnd) != self.monitor {
            return None;
        }
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(HWND(hwnd as _), Some(&mut pid)) };
        (pid != 0).then_some(logic::PreviewPress {
            hwnd,
            pid,
            close,
            middle,
        })
    }

    fn hide_zoom(&mut self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), PREVIEW_ZOOM_TIMER);
            if let Some(thumbnail) = self.zoom_thumbnail.take() {
                let _ = windows::Win32::Graphics::Dwm::DwmUnregisterThumbnail(thumbnail);
            }
            let _ = ShowWindow(self.zoom, SW_HIDE);
        }
        self.zoom_source = None;
    }

    /// A public DWM thumbnail, never a foreground/visibility change to the app.
    fn show_zoom(&mut self) {
        use windows::Win32::Graphics::Dwm::*;
        unsafe {
            let _ = KillTimer(Some(self.hwnd), PREVIEW_ZOOM_TIMER);
        }
        // Layered + transparent is the documented cross-process click-through
        // contract. Use redirected painting; this window never calls ULW.
        if unsafe { SetLayeredWindowAttributes(self.zoom, COLORREF(0), 255, LWA_ALPHA) }.is_err() {
            self.hide_zoom();
            return;
        }
        let Some((row, false)) = self.windows_hover else { return };
        let Some(&source) = self.windows_targets.get(row) else {
            return;
        };
        if self.preview_current_monitor_only && monitor_for_window(source) != self.monitor {
            self.hide_zoom();
            return;
        }
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(HWND(source as _), Some(&mut pid)) };
        if pid == 0 || self.fullscreen || self.suspended || !self.visible {
            self.hide_zoom();
            return;
        }
        self.hide_zoom();
        let mut popup = RECT::default();
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        unsafe {
            if GetWindowRect(self.windows, &mut popup).is_err() || !GetMonitorInfoW(self.monitor, &mut info).as_bool() {
                return;
            }
        }
        let scale = self.dpi as f32 / 96.;
        let gap = (8. * scale).round() as i32;
        let width = (560. * scale)
            .round()
            .min((info.rcWork.right - info.rcWork.left - 2 * gap) as f32) as i32;
        let height = (340. * scale)
            .round()
            .min((popup.top - info.rcWork.top - 2 * gap) as f32) as i32;
        if width < (120. * scale) as i32 || height < (100. * scale) as i32 {
            return;
        }
        let x = ((popup.left + popup.right - width) / 2).clamp(info.rcWork.left + gap, info.rcWork.right - gap - width);
        let Ok(thumbnail) = (unsafe { DwmRegisterThumbnail(self.zoom, HWND(source as _)) }) else {
            return;
        };
        self.zoom_thumbnail = Some(thumbnail);
        self.zoom_source = Some((source, pid));
        self.zoom_title = self.windows_rows.get(row).cloned().unwrap_or_default();
        unsafe {
            let _ = SetWindowPos(
                self.zoom,
                Some(HWND_TOPMOST),
                x,
                popup.top - gap - height,
                width,
                height,
                SWP_NOACTIVATE,
            );
            crate::theme::shape(self.zoom, width, height, 8., self.dpi);
        }
        let result = (|| -> Result<()> {
            let size = unsafe { DwmQueryThumbnailSourceSize(thumbnail)? };
            if size.cx <= 0 || size.cy <= 0 {
                return Err(Error::from_thread());
            }
            let pad = (8. * scale).round() as i32;
            let top = (36. * scale).round() as i32;
            let factor = ((width - 2 * pad) as f32 / size.cx as f32).min((height - top - pad) as f32 / size.cy as f32);
            let w = (size.cx as f32 * factor).round() as i32;
            let h = (size.cy as f32 * factor).round() as i32;
            let left = (width - w) / 2;
            let top = top + (height - top - pad - h) / 2;
            unsafe {
                DwmUpdateThumbnailProperties(
                    thumbnail,
                    &DWM_THUMBNAIL_PROPERTIES {
                        dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE | DWM_TNP_OPACITY,
                        rcDestination: RECT {
                            left,
                            top,
                            right: left + w,
                            bottom: top + h,
                        },
                        opacity: 255,
                        fVisible: BOOL(1),
                        ..Default::default()
                    },
                )?;
            }
            self.zoom_renderer
                .draw_window_zoom(self.zoom, self.dpi, &self.zoom_palette, &self.zoom_title)?;
            Ok(())
        })();
        if result.is_err() {
            self.hide_zoom();
            return;
        }
        unsafe {
            let _ = ShowWindow(self.zoom, SW_SHOWNOACTIVATE);
            let _ = InvalidateRect(Some(self.zoom), None, false);
        }
    }

    /// Includes the short passage between the preview and its source icon.
    fn over_preview(&self, cursor: POINT) -> bool {
        let Some(slot) = self.windows_slot else { return false };
        let mut popup = RECT::default();
        if self.windows_rows.is_empty() || unsafe { GetWindowRect(self.windows, &mut popup) }.is_err() {
            return false;
        }
        let anchor = self.item_rect(slot);
        let inset = (10 * self.dpi / 96) as i32;
        let inside =
            |rect: RECT| (rect.left..rect.right).contains(&cursor.x) && (rect.top..rect.bottom).contains(&cursor.y);
        inside(popup)
            || inside(anchor)
            || ((popup.left.min(anchor.left)..popup.right.max(anchor.right)).contains(&cursor.x)
                && (popup.bottom..anchor.top + inset).contains(&cursor.y))
    }

    /// Keeps the list a moment while the pointer crosses to it, then hides it.
    fn leave_windows(&mut self) {
        if self.windows_slot.is_none() {
            return;
        }
        let shown = unsafe { IsWindowVisible(self.windows) }.as_bool();
        if !shown {
            // It never appeared, so a pending hover is now stale.
            self.hide_windows();
            return;
        }
        unsafe {
            let _ = SetTimer(Some(self.hwnd), WINDOWS_HIDE_TIMER, WINDOWS_HIDE_MS, None);
        }
    }

    /// Draws the hovered app's windows above its icon.
    fn show_windows(&mut self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), WINDOWS_TIMER);
        }
        if self.native_menu_open
            || !self.visible
            || self.suspended
            || self.fullscreen
            || !self.revealed
            || self.slide_to > 0.5
        {
            self.hide_windows();
            return;
        }
        let Some(slot) = self.windows_slot else { return };
        let Some(item) = self.item_at(slot) else {
            self.hide_windows();
            return;
        };
        if !self.windows_clicked && !logic::show_window_list(self.window_list_mode, self.preview_windows(item).len()) {
            self.hide_windows();
            return;
        }
        let mut windows = self.preview_windows(item);
        if windows.len() > 1 {
            windows.sort_by_cached_key(|window| monitor_for_window(window.hwnd) != self.monitor);
        }
        let current: Vec<_> = windows.iter().map(|window| window.hwnd).collect();
        let targets = logic::preview_order(&self.windows_order, &current);
        let rows: Vec<String> = targets
            .iter()
            .filter_map(|target| windows.iter().find(|window| window.hwnd == *target))
            .map(|window| logic::preview_title(&window.title, &window.exe, &item.app.name))
            .collect();
        if rows.is_empty() {
            self.hide_windows();
            return;
        }
        self.windows_hover = None;
        self.layout_windows(slot, rows, targets);
    }

    /// Updates titles by handle, preserving the displayed order and click targets
    /// even when a foreground change has reordered the app's windows.
    fn refresh_window_titles(&mut self) {
        let Some(slot) = self.windows_slot.filter(|_| !self.windows_rows.is_empty()) else {
            return;
        };
        let Some(item) = self.item_at(slot) else { return };
        let mut rows = self.windows_rows.clone();
        for (row, target) in rows.iter_mut().zip(&self.windows_targets) {
            let Some(window) = item.windows.iter().find(|window| window.hwnd == *target) else {
                self.hide_windows();
                return;
            };
            *row = logic::preview_title(&window.title, &window.exe, &item.app.name);
        }
        let monitors: Vec<_> = self
            .windows_targets
            .iter()
            .map(|&target| monitor_label(target))
            .collect();
        if rows != self.windows_rows || monitors != self.windows_monitors {
            self.windows_rows = rows;
            self.windows_monitors = monitors;
            unsafe {
                let _ = InvalidateRect(Some(self.windows), None, false);
            }
        }
    }

    /// Shows one page of thumbnails, keeping each card tied to its window handle.
    fn layout_windows(&mut self, slot: usize, rows: Vec<String>, targets: Vec<isize>) {
        self.hide_zoom();
        self.windows_press = None;
        self.windows_order = targets.clone();
        let scale = self.dpi as f32 / 96.;
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !unsafe { GetMonitorInfoW(MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST), &mut info) }.as_bool() {
            self.hide_windows();
            return;
        }
        let anchor = self.item_rect(slot);
        let gap = (6. * scale).round() as i32;
        let work = info.rcWork;
        let available_width = (work.right - work.left - 2 * gap) as f32 / scale - 2. * logic::PREVIEW_PAD;
        if available_width < 70. {
            self.hide_windows();
            return;
        }
        let card = logic::PREVIEW_WIDTH.min(available_width);
        let count = rows.len();
        let capacity = ((available_width / card).floor() as usize).clamp(1, 6);
        let paged = count > capacity;
        let footer = if paged { logic::PREVIEW_FOOTER } else { 0. };
        let available_height = (anchor.top - work.top - 2 * gap) as f32 / scale;
        let chrome = 2. * logic::PREVIEW_PAD + logic::PREVIEW_HEADER + self.monitor_caption_height() + footer;
        if available_height < chrome {
            self.hide_windows();
            return;
        }
        self.preview_height = (available_height - chrome).min(logic::PREVIEW_HEIGHT);
        self.preview_width = card;
        self.preview_count = count.div_ceil(capacity);
        self.preview_page = self.preview_page.min(self.preview_count.saturating_sub(1));
        let offset = self.preview_page * capacity;
        self.clear_preview();
        self.windows_rows = rows.into_iter().skip(offset).take(capacity).collect();
        self.windows_targets = targets.into_iter().skip(offset).take(capacity).collect();
        self.windows_monitors = self
            .windows_targets
            .iter()
            .map(|&target| monitor_label(target))
            .collect();
        let width = ((card * count.min(capacity) as f32 + 2. * logic::PREVIEW_PAD) * scale).ceil() as i32;
        let height = ((chrome + self.preview_height) * scale).ceil() as i32;
        let x = ((anchor.left + anchor.right - width) / 2)
            .min(work.right - gap - width)
            .max(work.left + gap);
        self.hide_tip();
        self.windows_hover = None;
        unsafe {
            crate::theme::shape(self.windows, width, height, 8., self.dpi);
            let _ = SetWindowPos(
                self.windows,
                Some(HWND_TOPMOST),
                x,
                anchor.top - gap - height,
                width,
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            let _ = InvalidateRect(Some(self.windows), None, false);
        }
        self.update_previews();
    }

    fn change_preview_page(&mut self, forward: bool) {
        let next = if forward {
            (self.preview_page + 1).min(self.preview_count.saturating_sub(1))
        } else {
            self.preview_page.saturating_sub(1)
        };
        if next != self.preview_page {
            self.preview_page = next;
            self.show_windows();
        }
    }

    fn monitor_caption_height(&self) -> f32 {
        if unsafe { GetSystemMetrics(SM_CMONITORS) } > 1 {
            logic::PREVIEW_MONITOR
        } else {
            0.
        }
    }

    /// Returns the card and whether its close button was hit.
    fn windows_hit(&self, lp: LPARAM) -> Option<(usize, bool)> {
        let scale = self.dpi as f32 / 96.;
        logic::preview_hit(
            (lp.0 as i16) as f32 / scale,
            ((lp.0 >> 16) as i16) as f32 / scale,
            self.preview_width,
            self.preview_height + self.monitor_caption_height(),
            self.windows_targets.len(),
        )
    }

    fn hide_tip(&mut self) {
        self.tip_target = None;
        self.desktop_pill = None;
        unsafe {
            let _ = KillTimer(Some(self.hwnd), DESKTOP_TIMER);
            let _ = KillTimer(Some(self.hwnd), TIP_TIMER);
            let _ = ShowWindow(self.tip, SW_HIDE);
        }
    }

    /// Shows the tooltip above what the pointer rests on.
    fn show_tip(&mut self) {
        if self.native_menu_open {
            return;
        }
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIP_TIMER);
        }
        let (text, anchor) = match self.tip_target {
            Some(tray::Tip::Chevron) => ("Show hidden icons".to_string(), self.part_rect(logic::Part::Chevron)),
            Some(tray::Tip::Inline(i)) => (
                self.inline
                    .get(i)
                    .map(|&i| self.tray[i].icon.tip.clone())
                    .unwrap_or_default(),
                self.part_rect(logic::Part::Tray(i)),
            ),
            Some(tray::Tip::Flyout(i)) => (
                self.tray.get(i).map(|shown| shown.icon.tip.clone()).unwrap_or_default(),
                Some(self.cell_rect(i)),
            ),
            None => return,
        };
        let text = text.trim().to_string();
        let Some(anchor) = anchor.filter(|_| !text.is_empty()) else {
            return;
        };
        let scale = self.dpi as f32 / 96.0;
        let Ok((width, height)) = self.tip_renderer.measure_tip(&text, TIP_MAX) else {
            return;
        };
        let (width, height) = ((width * scale).ceil() as i32, (height * scale).ceil() as i32);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        unsafe {
            let _ = GetMonitorInfoW(MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST), &mut info);
        }
        let gap = (6.0 * scale).round() as i32;
        let x = ((anchor.left + anchor.right - width) / 2)
            .min(info.rcMonitor.right - gap - width)
            .max(info.rcMonitor.left + gap);
        self.tip_text = text;
        unsafe {
            let _ = SetWindowPos(
                self.tip,
                Some(HWND_TOPMOST),
                x,
                anchor.top - gap - height,
                width,
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            let _ = InvalidateRect(Some(self.tip), None, false);
        }
    }

    fn hit(&self, lp: LPARAM) -> Option<usize> {
        let scale = self.dpi as f32 / 96.0 * self.animation_scale();
        let running = self.count();
        logic::hit_scaled(
            (lp.0 as i16) as f32 / scale,
            ((lp.0 >> 16) as i16) as f32 / scale,
            running,
            logic::layout_scale(self.app_icon_size),
        )
    }

    fn hit_part(&self, lp: LPARAM) -> Option<logic::Part> {
        let scale = self.dpi as f32 / 96.0 * self.animation_scale();
        let running = self.count();
        logic::part_hit_scaled(
            (lp.0 as i16) as f32 / scale,
            ((lp.0 >> 16) as i16) as f32 / scale,
            running,
            &self.parts(),
            logic::layout_scale(self.app_icon_size),
        )
    }

    fn sound_open(&self) -> bool {
        self.sound_identity.is_some_and(sound::is_current)
    }

    fn close_sound(&mut self) {
        self.sound_epoch = self.sound_epoch.wrapping_add(1);
        if let Some(id) = self.sound_identity.take() {
            sound::defer_destroy(id);
        }
        self.sound = HWND::default();
    }

    fn prepare_sound(&mut self) -> Option<(u64, sound::OpenRequest)> {
        let was_open = self.sound_open();
        self.close_sound();
        if was_open {
            return None;
        }
        let anchor = self.part_rect(logic::Part::Volume)?;
        self.close_flyout();
        self.hide_tip();
        self.hide_windows();
        Some((
            self.sound_epoch,
            sound::OpenRequest {
                placement: sound::Placement {
                    owner: self.hwnd,
                    launcher: self.owner,
                    anchor,
                    dpi: self.dpi,
                    entry: sound::Entry::Home,
                    below: false,
                },
                state: self.sound_state.clone(),
                hardware: self.sound_hardware.clone(),
                renderer: self.flyout_renderer.for_window(),
                material: (
                    self.dark,
                    self.backdrop,
                    self.backdrop_color.clone(),
                    self.material_options,
                ),
                open_ms: self.open_ms,
                remote: self.remote,
            },
        ))
    }

    /// The speaker opens Sound; the clock opens the calendar and notifications;
    /// the battery opens its settings. The chevron toggles the flyout, and
    /// notification icons get the click.
    fn click_part(&mut self, part: logic::Part) -> DockEffect {
        let target = match part {
            logic::Part::Chevron => {
                self.toggle_flyout();
                return DockEffect::None;
            }
            logic::Part::Tray(position) => {
                self.click_inline(position, Action::Click);
                return DockEffect::None;
            }
            logic::Part::Volume => {
                return DockEffect::Controls;
            }
            logic::Part::Clock => "ms-actioncenter:",
            logic::Part::Battery => "ms-settings:batterysaver",
        };
        DockEffect::Open(target)
    }

    /// Wheel `delta` over the dock switches desktops a notch at a time,
    /// negative toward the right.
    fn wheel_desktops(&mut self, delta: i32) {
        if self.desktop_switched.is_some_and(|at| at.elapsed() < DESKTOP_PAUSE) {
            self.desktop_travel = 0;
            return;
        }
        let (step, travel) = logic::wheel_desktop(self.desktop_travel, delta);
        self.desktop_travel = travel;
        let Some(right) = step else { return };
        // Explorer updates its list after the switch, so a showing pill knows better.
        let mut desktops = self.desktop_pill.take().unwrap_or_else(desktops::read);
        let target = if right {
            desktops.current + 1
        } else {
            desktops.current.wrapping_sub(1)
        };
        // Like Windows, the ends don't wrap around. The pill still shows where you are.
        if target < desktops.names.len() {
            desktops::switch(right);
            desktops.current = target;
            self.desktop_switched = Some(Instant::now());
        }
        self.show_desktops(desktops);
    }

    /// Shows the desktops in a pill above the dock for a moment.
    fn show_desktops(&mut self, desktops: desktops::Desktops) {
        self.hide_windows();
        self.tip_target = None;
        let name = &desktops.names[desktops.current];
        let Ok((width, height)) = self.tip_renderer.measure_desktops(desktops.names.len(), name) else {
            return;
        };
        let scale = self.dpi as f32 / 96.0;
        let (width, height) = ((width * scale).ceil() as i32, (height * scale).ceil() as i32);
        let gap = (6.0 * scale).round() as i32;
        self.desktop_pill = Some(desktops);
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIP_TIMER);
            let mut dock = RECT::default();
            let _ = GetWindowRect(self.hwnd, &mut dock);
            let _ = SetWindowPos(
                self.tip,
                Some(HWND_TOPMOST),
                (dock.left + dock.right - width) / 2,
                dock.top - gap - height,
                width,
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            let _ = InvalidateRect(Some(self.tip), None, false);
            if !self.held {
                SetTimer(Some(self.hwnd), DESKTOP_TIMER, DESKTOP_PILL_MS, None);
            }
        }
    }

    /// Wheel `delta` over the speaker changes the volume by 2% a notch.
    fn wheel_volume(&mut self, delta: i32) {
        let Some(volume) = self.status.volume else { return };
        let (percent, rest) = logic::wheel_volume(volume.percent, self.wheel + delta);
        self.wheel = rest;
        if percent != volume.percent
            && let Err(error) = crate::commands::audio::set_volume(percent)
        {
            eprintln!("Dock volume: {error}");
        }
        self.update_status();
    }

    /// Rereads the status, redrawing if it changed.
    fn update_status(&mut self) {
        let _span = crate::util::perf::Span::new("status");
        self.status_read_at = Instant::now();
        let status = status::read();
        if status != self.status {
            self.status = status;
            unsafe {
                let _ = InvalidateRect(Some(self.hwnd), None, false);
            }
        }
    }

    fn drop_target(&self, source: &str, lp: LPARAM) -> Option<(usize, &Item)> {
        let slot = self.hit(lp)?;
        let item = self.item_at(slot)?;
        (!item.app.id.eq_ignore_ascii_case(source)
            && self.data.pins.borrow().position(source).is_some()
                == self.data.pins.borrow().position(&item.app.id).is_some())
        .then_some((slot, item))
    }

    fn drag_frame(&self) -> Option<render::DragFrame> {
        let press = self.app_press.as_ref().filter(|press| press.dragging)?;
        let from = (1..=self.count()).find(|&slot| {
            self.item_at(slot)
                .is_some_and(|item| item.app.id.eq_ignore_ascii_case(&press.id))
        })?;
        let pointer = press.pointer?;
        Some(render::DragFrame {
            from,
            to: self.drop_target(&press.id, pointer).map(|(slot, _)| slot),
            offset: (mouse_point(pointer).x - press.start.x) as f32 / (self.dpi as f32 / 96. * self.animation_scale()),
        })
    }

    fn reorder_app(&mut self, id: &str, target: &str) {
        if self.data.pins.borrow().position(id).is_some() {
            if let Err(error) = self.data.pins.borrow_mut().reorder(id, target) {
                eprintln!("Dock pins: {error}");
            } else {
                post_to_docks(WM_REFRESH, Some(self.hwnd));
            }
            self.refresh();
        } else if let (Some(from), Some(to)) = (self.app_index(id), self.app_index(target)) {
            let item = self.items.remove(from);
            self.items.insert(to, item);
        }
    }

    fn app_index(&self, id: &str) -> Option<usize> {
        self.items.iter().position(|item| item.app.id.eq_ignore_ascii_case(id))
    }

    fn cancel_app_press(&mut self) {
        if self.app_press.take().is_some() {
            unsafe {
                if GetCapture() == self.hwnd {
                    let _ = ReleaseCapture();
                }
                let _ = InvalidateRect(Some(self.hwnd), None, false);
            }
            self.hover = None;
            self.animate_hover();
        }
    }

    fn click(&mut self, index: usize, new_instance: bool) -> DockEffect {
        if !self.alive || !self.data.dock_enabled.get() || self.suspended {
            return DockEffect::None;
        }
        if index == 0 {
            unsafe {
                let _ = PostMessageW(Some(self.owner), WM_SHOW_LAUNCHER, WPARAM(0), LPARAM(0));
            }
        } else if !new_instance
            && self
                .item_at(index)
                .is_some_and(|item| self.preview_windows(item).len() > 1)
        {
            if self.windows_slot == Some(index) && self.windows_clicked {
                self.hide_windows();
            } else {
                if self.windows_slot != Some(index) {
                    self.hide_windows();
                }
                self.windows_slot = Some(index);
                self.windows_clicked = true;
                if self.windows_targets.is_empty() {
                    self.show_windows();
                }
            }
        } else if let Some(item) = self.items.get(index - 1 + self.scroll) {
            let local = self.preview_windows(item);
            let windows: Vec<_> = if local.is_empty() {
                item.windows.iter().collect()
            } else {
                local
            };
            if !new_instance && let Some(window) = windows.first() {
                if windows.iter().any(|window| open_windows::is_foreground(window.hwnd)) {
                    return DockEffect::Minimize(
                        windows
                            .iter()
                            .filter_map(|window| window_identity(window.hwnd))
                            .collect(),
                    );
                }
                return window_identity(window.hwnd).map_or(DockEffect::None, DockEffect::Focus);
            }
            return DockEffect::Launch {
                app: item.app.clone(),
                indexed: self
                    .apps
                    .iter()
                    .any(|indexed| indexed.id.eq_ignore_ascii_case(&item.app.id)),
            };
        }
        DockEffect::None
    }

    fn prepare_menu(&mut self, index: Option<usize>) -> Option<DockMenu> {
        if self.native_menu_open {
            return None;
        }
        let snapshot = if let Some(index) = index {
            let item = self.item_at(index)?;
            DockMenu::App {
                hwnd: self.hwnd,
                app: item.app.clone(),
                pinned: self.data.pins.borrow().position(&item.app.id).is_some(),
                windows: item
                    .windows
                    .iter()
                    .filter_map(|window| {
                        let mut pid = 0;
                        unsafe { GetWindowThreadProcessId(HWND(window.hwnd as _), Some(&mut pid)) };
                        (pid != 0).then_some((window.hwnd, pid))
                    })
                    .collect(),
            }
        } else {
            DockMenu::Chevron {
                hwnd: self.hwnd,
                owner: self.owner,
            }
        };
        self.native_menu_open = true;
        self.hide_tip();
        self.hide_windows();
        Some(snapshot)
    }

    fn apply_menu(&mut self, snapshot: DockMenu, command: u32) -> DockEffect {
        self.native_menu_open = false;
        self.away = 0;
        if !self.alive
            || !self.data.dock_enabled.get()
            || self.suspended
            || !unsafe { IsWindow(Some(self.hwnd)) }.as_bool()
        {
            return DockEffect::None;
        }
        let effect = match snapshot {
            DockMenu::Chevron { owner, .. } => match command {
                1 => DockEffect::Taskbar(owner),
                2 => DockEffect::Open("ms-settings:taskbar"),
                _ => DockEffect::None,
            },
            DockMenu::App {
                app, pinned, windows, ..
            } => {
                // Reentrant updates can reorder the dock or replace a row.
                // Resolve by stable app ID, never by the original menu slot.
                let app = self
                    .items
                    .iter()
                    .find(|item| item.app.id.eq_ignore_ascii_case(&app.id))
                    .map_or(app.clone(), |item| item.app.clone());
                match command {
                    1 => DockEffect::Launch {
                        indexed: self.apps.iter().any(|entry| entry.id.eq_ignore_ascii_case(&app.id)),
                        app,
                    },
                    2 => {
                        if let Err(error) = self.data.pins.borrow_mut().set_pinned(app, !pinned) {
                            eprintln!("Dock pins: {error}");
                        } else {
                            post_to_docks(WM_REFRESH, Some(self.hwnd));
                        }
                        DockEffect::None
                    }
                    3 => DockEffect::Close(windows),
                    _ => DockEffect::None,
                }
            }
        };
        let _ = unsafe { PostMessageW(Some(self.hwnd), WM_REFRESH, WPARAM(0), LPARAM(0)) };
        effect
    }
}

/// GWLP_USERDATA points into this Rc allocation. Retain it before entering a
/// modal loop, because a reentrant config update may remove the owning Dock.
unsafe fn retain_dock_state(pointer: *const RefCell<State>) -> Rc<RefCell<State>> {
    unsafe {
        Rc::increment_strong_count(pointer);
        Rc::from_raw(pointer)
    }
}

/// Preserve stacking while either our native menu or a tray menu owns it.
fn dock_menu_order(native_menu_open: bool, tray_interaction: bool) -> SET_WINDOW_POS_FLAGS {
    if native_menu_open || tray_interaction {
        SWP_NOZORDER
    } else {
        SET_WINDOW_POS_FLAGS(0)
    }
}

fn display_dock_menu(snapshot: &DockMenu) -> u32 {
    let kind = match snapshot {
        DockMenu::App { .. } => "app",
        DockMenu::Chevron { .. } => "tray",
    };
    crate::logging::event("dock.menu.open", format_args!("kind={kind}"));
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return 0 };
        let hwnd = match snapshot {
            DockMenu::Chevron { hwnd, .. } => {
                let _ = AppendMenuW(menu, MF_STRING, 1, w!("Show taskbar"));
                let _ = AppendMenuW(menu, MF_STRING, 2, w!("Taskbar settings"));
                *hwnd
            }
            DockMenu::App {
                hwnd, pinned, windows, ..
            } => {
                let _ = AppendMenuW(menu, MF_STRING, 1, w!("New window"));
                let _ = AppendMenuW(
                    menu,
                    MF_STRING,
                    2,
                    if *pinned {
                        w!("Unpin from dock")
                    } else {
                        w!("Pin to dock")
                    },
                );
                if !windows.is_empty() {
                    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
                    let _ = AppendMenuW(
                        menu,
                        MF_STRING,
                        3,
                        if windows.len() == 1 {
                            w!("Close window")
                        } else {
                            w!("Close all windows")
                        },
                    );
                }
                *hwnd
            }
        };
        let mut point = POINT::default();
        let _ = GetCursorPos(&mut point);
        // Open upwards from the dock edge, so every row clears the dock.
        let mut dock = RECT::default();
        let menu_bottom = match GetWindowRect(hwnd, &mut dock) {
            Ok(()) => dock.top,
            Err(error) => {
                eprintln!("Dock context menu position: {error}");
                point.y
            }
        };
        let _ = SetForegroundWindow(hwnd);
        let command = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
            point.x,
            menu_bottom,
            None,
            hwnd,
            None,
        )
        .0 as u32;
        let _ = DestroyMenu(menu);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        crate::logging::event("dock.menu.close", format_args!("kind={kind} selected={}", command != 0));
        command
    }
}

fn run_dock_menu(cell: &RefCell<State>, index: Option<usize>) {
    let effect = modal_action(
        cell,
        |state| state.prepare_menu(index),
        display_dock_menu,
        |state, snapshot, command| state.apply_menu(snapshot, command),
    );
    run_dock_effect(cell, effect.unwrap_or(DockEffect::None));
}

fn window_identity(target: isize) -> Option<(isize, u32)> {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(HWND(target as _), Some(&mut pid)) };
    (pid != 0).then_some((target, pid))
}

fn unborrowed_action<S, T, R>(cell: &RefCell<S>, prepare: impl FnOnce(&mut S) -> T, execute: impl FnOnce(T) -> R) -> R {
    let action = { prepare(&mut cell.borrow_mut()) };
    execute(action)
}

fn run_dock_click(cell: &RefCell<State>, index: usize, new_instance: bool) {
    unborrowed_action(
        cell,
        |state| state.click(index, new_instance),
        |effect| run_dock_effect(cell, effect),
    );
}

fn dock_effect_allowed(cell: &RefCell<State>) -> bool {
    let state = cell.borrow();
    state.alive && state.data.dock_enabled.get() && !state.suspended && unsafe { IsWindow(Some(state.hwnd)) }.as_bool()
}

fn run_dock_effect(cell: &RefCell<State>, effect: DockEffect) {
    if !dock_effect_allowed(cell) {
        return;
    }
    match effect {
        DockEffect::Controls => {
            unborrowed_action(cell, State::prepare_sound, |request| {
                let Some((epoch, request)) = request else { return };
                // Native creation, presentation and focus may dispatch status
                // updates or remove this owner. No owner borrow crosses them.
                match request.open() {
                    Ok(hwnd) => {
                        let id = sound::identity(hwnd);
                        let allowed = dock_effect_allowed(cell);
                        let mut state = cell.borrow_mut();
                        if allowed && state.sound_epoch == epoch {
                            state.sound = hwnd;
                            state.sound_identity = id;
                            if let (Some(id), Some(anchor)) = (id, state.part_rect(logic::Part::Volume)) {
                                sound::update_opener(id, anchor);
                            }
                            state.away = 0;
                            let _ = unsafe { InvalidateRect(Some(state.hwnd), None, false) };
                        } else if let Some(id) = id {
                            drop(state);
                            sound::destroy(id);
                        }
                    }
                    Err(error) => {
                        crate::logging::event("controls.open.failed", format_args!("owner=dock error={error}"))
                    }
                }
            });
        }

        DockEffect::Focus((target, pid)) => {
            if window_identity(target) == Some((target, pid))
                && let Err(error) = open_windows::focus(target)
            {
                eprintln!("Dock activation: {error}");
            }
        }
        DockEffect::Minimize(windows) => {
            for (target, pid) in windows {
                if !dock_effect_allowed(cell) {
                    break;
                }
                if window_identity(target) == Some((target, pid))
                    && let Err(error) = open_windows::minimize(target)
                {
                    eprintln!("Dock minimize: {error}");
                }
            }
        }
        DockEffect::None => {}
        DockEffect::Launch { app, indexed } => launch_dock_app(&app, indexed),
        DockEffect::Close(windows) => {
            for (target, expected_pid) in windows {
                if !dock_effect_allowed(cell) {
                    break;
                }
                let mut pid = 0;
                unsafe { GetWindowThreadProcessId(HWND(target as _), Some(&mut pid)) };
                if pid == expected_pid
                    && let Err(error) = open_windows::close(target)
                {
                    eprintln!("Dock close: {error}");
                }
            }
        }
        DockEffect::Taskbar(owner) => {
            let _ = unsafe { PostMessageW(Some(owner), WM_DOCK_SHOW_TASKBAR, WPARAM(0), LPARAM(0)) };
        }
        DockEffect::Open(target) => {
            if let Err(error) = crate::launch::open(target) {
                eprintln!("Dock status: {error}");
            }
        }
    }
}

fn launch_dock_app(app: &AppEntry, indexed: bool) {
    let result = if indexed {
        crate::launch::app(&app.id, app.path.as_deref(), false)
    } else if let Some(path) = &app.path {
        if !std::path::Path::new(path).is_absolute() {
            return;
        }
        crate::launch::open(path)
    } else {
        crate::launch::app(&app.id, None, false)
    };
    if let Err(error) = result {
        eprintln!("Dock launch: {error}");
    }
}

#[cfg(test)]
mod dock_modal_tests {
    use super::{dock_menu_order, modal_action, unborrowed_action};
    use std::cell::RefCell;
    use std::rc::Rc;
    use windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER;

    #[derive(Default)]
    struct Model {
        apps: Vec<&'static str>,
        wifi: bool,
        sound_version: u32,
        applied: Option<&'static str>,
        alive: bool,
    }

    #[test]
    fn native_menu_preserves_z_order_across_updates_and_tray_completion() {
        // A tray interaction may expire during our nested native menu loop.
        assert_eq!(dock_menu_order(true, true), SWP_NOZORDER);
        assert_eq!(dock_menu_order(true, false), SWP_NOZORDER);
        assert_eq!(dock_menu_order(false, true), SWP_NOZORDER);
        assert_eq!(dock_menu_order(false, false).0, 0);
        let cell = RefCell::new(false);
        modal_action(
            &cell,
            |open| {
                *open = true;
                Some(())
            },
            |()| {
                // The real modal boundary permits periodic placement updates.
                assert_eq!(dock_menu_order(*cell.borrow(), false), SWP_NOZORDER);
                0
            },
            |open, (), _| *open = false,
        );
        assert_eq!(dock_menu_order(*cell.borrow(), false).0, 0);
    }

    #[test]
    fn controls_open_boundary_accepts_status_updates_and_owner_removal() {
        let owner = RefCell::new(Some(Rc::new(RefCell::new(Model {
            alive: true,
            sound_version: 3,
            ..Default::default()
        }))));
        let retained = owner.borrow().as_ref().unwrap().clone();
        let weak = Rc::downgrade(&retained);
        let prepared_version = unborrowed_action(
            &retained,
            |state| state.sound_version,
            |version| {
                // Inject the native open/focus boundary: status callbacks and
                // monitor/Remote-mode owner removal must both be possible here.
                retained.borrow_mut().wifi = true;
                owner.borrow_mut().take();
                retained.borrow_mut().alive = false;
                version
            },
        );
        assert_eq!(prepared_version, 3);
        assert!(retained.borrow().wifi);
        assert!(!retained.borrow().alive);
        assert!(weak.upgrade().is_some());
        drop(retained);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn ordinary_launch_executes_without_state_borrow_and_keeps_reentrant_status_updates() {
        let cell = RefCell::new(Model {
            apps: vec!["Editor", "Browser"],
            alive: true,
            ..Default::default()
        });
        let launched = unborrowed_action(
            &cell,
            |state| state.apps[0],
            |app| {
                // ShellExecute's native loop may deliver the same status/index
                // updates as TrackPopupMenu. The production effect boundary must
                // have released the click preparation borrow before this callback.
                let mut state = cell.borrow_mut();
                state.wifi = true;
                state.sound_version = 11;
                state.apps.reverse();
                app
            },
        );
        assert_eq!(launched, "Editor");
        assert!(cell.borrow().wifi);
        assert_eq!(cell.borrow().sound_version, 11);
        assert_eq!(cell.borrow().apps[0], "Browser");
    }

    #[test]
    fn modal_dispatch_accepts_reentrant_wifi_sound_and_index_updates_without_losing_them() {
        let cell = RefCell::new(Model {
            apps: vec!["Editor", "Browser"],
            alive: true,
            ..Default::default()
        });
        modal_action(
            &cell,
            |state| Some(state.apps[0]),
            |snapshot| {
                // This is the production modal helper's display boundary, where
                // TrackPopupMenu pumps confirm_wifi, sound and index events.
                let mut updated = cell.borrow_mut();
                updated.wifi = true;
                updated.sound_version = 7;
                updated.apps = vec!["Browser", "Editor", "New app"];
                assert_eq!(*snapshot, "Editor");
                1
            },
            |state, app_id, command| {
                if command != 0 {
                    state.applied = state.apps.iter().copied().find(|id| id.eq_ignore_ascii_case(app_id));
                }
            },
        );
        let current = cell.borrow();
        assert!(current.wifi);
        assert_eq!(current.sound_version, 7);
        assert_eq!(current.apps.len(), 3);
        assert_eq!(
            current.applied,
            Some("Editor"),
            "the reordered first slot must not receive the result"
        );
    }

    #[test]
    fn canceled_menu_preserves_nested_updates_and_removed_owner_stays_alive_until_return() {
        let owner = RefCell::new(Some(Rc::new(RefCell::new(Model {
            alive: true,
            ..Default::default()
        }))));
        let retained = owner.borrow().as_ref().unwrap().clone();
        let weak = Rc::downgrade(&retained);
        modal_action(
            &retained,
            |_| Some(()),
            |_| {
                owner.borrow_mut().take();
                let mut state = retained.borrow_mut();
                state.wifi = true;
                state.alive = false;
                1
            },
            |state, (), command| {
                if state.alive && command != 0 {
                    state.applied = Some("wrong");
                }
            },
        );
        assert!(retained.borrow().wifi);
        assert_eq!(retained.borrow().applied, None);
        modal_action(
            &retained,
            |_| Some(()),
            |_| {
                retained.borrow_mut().sound_version = 9;
                0
            },
            |state, (), command| {
                if command != 0 {
                    state.applied = Some("wrong");
                }
            },
        );
        assert_eq!(retained.borrow().sound_version, 9);
        assert_eq!(retained.borrow().applied, None);
        drop(retained);
        assert!(weak.upgrade().is_none());
    }
}

fn mouse_point(lp: LPARAM) -> POINT {
    POINT {
        x: (lp.0 as i16).into(),
        y: ((lp.0 >> 16) as i16).into(),
    }
}

fn monitor_label(target: isize) -> String {
    if unsafe { GetSystemMetrics(SM_CMONITORS) } <= 1 {
        return String::new();
    }
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    let monitor = unsafe { MonitorFromWindow(HWND(target as _), MONITOR_DEFAULTTONULL) };
    if !unsafe { GetMonitorInfoW(monitor, &mut info.monitorInfo) }.as_bool() {
        return String::new();
    }
    let name = String::from_utf16_lossy(
        &info.szDevice[..info
            .szDevice
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(info.szDevice.len())],
    );
    name.strip_prefix(r"\\.\DISPLAY")
        .filter(|number| !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
        .map_or_else(String::new, |number| {
            let primary = unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) };
            let mut primary_info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let side = if monitor == primary {
                "Main"
            } else if unsafe { GetMonitorInfoW(primary, &mut primary_info) }.as_bool() {
                let main = primary_info.rcMonitor;
                let rect = info.monitorInfo.rcMonitor;
                if rect.right <= main.left {
                    "Left"
                } else if rect.left >= main.right {
                    "Right"
                } else if rect.bottom <= main.top {
                    "Above"
                } else if rect.top >= main.bottom {
                    "Below"
                } else {
                    ""
                }
            } else {
                ""
            };
            if side.is_empty() {
                format!("Display {number}")
            } else {
                format!("Display {number} · {side}")
            }
        })
}

fn window_aumid(hwnd: HWND) -> Option<String> {
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreForWindow(hwnd).ok()?;
        // PKEY_AppUserModel_ID, defined here to avoid an unrelated storage feature.
        let key = PROPERTYKEY {
            fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
            pid: 5,
        };
        let mut value = store.GetValue(&key).ok()?;
        let result = PropVariantToStringAlloc(&value);
        let _ = PropVariantClear(&mut value);
        let ptr = result.ok()?;
        let text = ptr.to_string().ok();
        CoTaskMemFree(Some(ptr.0.cast()));
        text.filter(|text| !text.is_empty())
    }
}

fn primary_monitor() -> HMONITOR {
    unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) }
}

fn monitor_info(monitor: HMONITOR) -> Option<MONITORINFO> {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool().then_some(info)
}

unsafe extern "system" fn collect_monitor(monitor: HMONITOR, _: HDC, _: *mut RECT, param: LPARAM) -> BOOL {
    unsafe { (*(param.0 as *mut Vec<HMONITOR>)).push(monitor) };
    true.into()
}

fn monitor_for_window(hwnd: isize) -> HMONITOR {
    unsafe { MonitorFromWindow(HWND(hwnd as _), MONITOR_DEFAULTTONEAREST) }
}

fn covers(outer: RECT, inner: RECT) -> bool {
    outer.left <= inner.left && outer.top <= inner.top && outer.right >= inner.right && outer.bottom >= inner.bottom
}

struct SurfaceWindow {
    bounds: RECT,
    raw: RECT,
    maximized: bool,
    borderless: bool,
    monitor: HMONITOR,
}

fn focused_fullscreen_on(window: Option<&SurfaceWindow>, monitor: RECT, work: RECT) -> bool {
    window.is_some_and(|window| fullscreen_on(std::slice::from_ref(window), monitor, work))
}

/// Soft auto-hide policy for local Smart mode, independent of hard fullscreen
/// suppression shared with the top bar, notch and fixed Remote layout.
fn focused_maximized_on(window: Option<&SurfaceWindow>, monitor: HMONITOR) -> bool {
    !monitor.is_invalid() && window.is_some_and(|window| window.maximized && window.monitor == monitor)
}

fn hover_keeps_dock_visible(
    fixed: bool,
    smart_clear: bool,
    bottom_strip: bool,
    interacting: bool,
    pointer_near: bool,
) -> bool {
    fixed || smart_clear || bottom_strip || interacting || pointer_near
}

fn borderless_surface(style: u32) -> bool {
    style & (WS_CAPTION.0 | WS_THICKFRAME.0) == 0
}

fn focused_surface() -> Option<SurfaceWindow> {
    // Read the current root afresh instead of consulting the shared 150 ms
    // z-order cache: background fullscreen windows must never block this dock.
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.is_invalid() {
        return None;
    }
    let root = unsafe { GetAncestor(foreground, GA_ROOT) };
    if root.is_invalid() {
        return None;
    }
    unsafe { read_surface(root) }
}

fn excluded_surface(pid: u32, own_pid: u32, class: &[u16]) -> bool {
    pid == own_pid
        || ["Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd"]
            .iter()
            .any(|name| class.iter().copied().eq(name.encode_utf16()))
}

fn fullscreen_on(windows: &[SurfaceWindow], monitor: RECT, work: RECT) -> bool {
    for window in windows {
        // RDP/display reconfiguration can temporarily leave no reserved work
        // area. A genuinely borderless full-monitor window remains fullscreen
        // there; a captioned/resizable maximized window still does not qualify.
        if covers(window.bounds, monitor)
            && (!window.maximized || (window.raw == monitor && (work != monitor || window.borderless)))
        {
            return true;
        }
        // A maximized foreground window can cover a fullscreen window lower in z-order.
        if window.maximized && covers(window.bounds, work) {
            break;
        }
    }
    false
}

#[cfg(test)]
mod visibility_tests {
    use super::*;

    fn surface(bounds: RECT, maximized: bool) -> SurfaceWindow {
        SurfaceWindow {
            bounds,
            raw: bounds,
            maximized,
            borderless: false,
            monitor: HMONITOR::default(),
        }
    }

    #[test]
    fn focused_maximized_uses_its_assigned_monitor_after_work_area_and_dpi_changes() {
        let left = HMONITOR(1 as _);
        let right = HMONITOR(2 as _);
        // Old 150%-scaled work area, negative coordinates and a one-pixel
        // inset must not defeat the authoritative maximized/monitor state.
        let bounds = RECT {
            left: -2879,
            top: -119,
            right: -1,
            bottom: 1480,
        };
        let mut focused = SurfaceWindow {
            monitor: left,
            ..surface(bounds, true)
        };
        assert!(focused_maximized_on(Some(&focused), left));
        assert!(!focused_maximized_on(Some(&focused), right));
        assert!(!focused_maximized_on(Some(&focused), HMONITOR::default()));
        focused.bounds.bottom = 1400;
        assert!(focused_maximized_on(Some(&focused), left));
        focused.monitor = right;
        assert!(!focused_maximized_on(Some(&focused), left));
        assert!(focused_maximized_on(Some(&focused), right));
    }

    #[test]
    fn restored_foreground_and_excluded_foreground_do_not_inherit_background_maximized_state() {
        let monitor = HMONITOR(1 as _);
        let bounds = RECT {
            right: 1920,
            bottom: 1040,
            ..Default::default()
        };
        let background = SurfaceWindow {
            monitor,
            ..surface(bounds, true)
        };
        let foreground = SurfaceWindow {
            monitor,
            ..surface(bounds, false)
        };
        assert!(focused_maximized_on(Some(&background), monitor));
        assert!(!focused_maximized_on(Some(&foreground), monitor));
        assert!(!focused_maximized_on(None, monitor));
    }

    #[test]
    fn maximized_smart_mode_retains_bottom_reveal_delay_holds_and_remote_fixed_policy() {
        let monitor = HMONITOR(1 as _);
        let bounds = RECT {
            left: -1920,
            top: -120,
            right: 0,
            bottom: 960,
        };
        let focused = SurfaceWindow {
            monitor,
            ..surface(RECT { bottom: 920, ..bounds }, true)
        };
        let saved = crate::config::Config {
            dock_autohide: true,
            dock_smart_hide: true,
            ..Default::default()
        };
        let local = saved.for_remote(false);
        let smart_clear = local.dock_smart_hide && !focused_maximized_on(Some(&focused), monitor);
        assert!(!smart_clear);
        assert!(!focused_fullscreen_on(Some(&focused), bounds, focused.bounds));
        for dpi in [96, 144, 192] {
            let area = (bounds.left, bounds.top, bounds.right, bounds.bottom);
            let (reveal, bottom) = logic::bottom_hover_zones((-1900, 959), area, dpi, 24., 15, false);
            assert!(logic::reveal_from_hover_strip(reveal, false));
            assert!(!logic::reveal_from_hover_strip(reveal, true));
            assert!(hover_keeps_dock_visible(false, smart_clear, bottom, false, false));
            let mut away = 0;
            for _ in 0..HIDE_AFTER {
                away = logic::hover_away_polls(away, hover_keeps_dock_visible(false, smart_clear, false, false, false));
            }
            assert_eq!(away, HIDE_AFTER);
            // Any active menu, preview, Start, held dock or drag resets the
            // existing away delay even with the pointer outside the dock.
            assert_eq!(
                logic::hover_away_polls(away, hover_keeps_dock_visible(false, smart_clear, false, true, false)),
                0
            );
            assert!(hover_keeps_dock_visible(false, smart_clear, false, false, true));
        }
        let remote = saved.for_remote(true);
        assert!(!remote.dock_autohide && !remote.dock_smart_hide);
        assert!(hover_keeps_dock_visible(
            !remote.dock_autohide,
            false,
            false,
            false,
            false
        ));
        let restored = saved.for_remote(false);
        assert!(restored.dock_autohide && restored.dock_smart_hide);
        assert!(!hover_keeps_dock_visible(
            !restored.dock_autohide,
            false,
            false,
            false,
            false
        ));
    }

    #[test]
    fn dock_ignores_background_fullscreen_when_foreground_is_not_fullscreen() {
        let monitor = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let work = RECT {
            bottom: 1040,
            ..monitor
        };
        let background = surface(monitor, false);
        let foreground = surface(
            RECT {
                left: 200,
                top: 100,
                right: 1200,
                bottom: 800,
            },
            false,
        );
        // The existing topbar/notch policy still observes the background window.
        assert!(fullscreen_on(&[background], monitor, work));
        assert!(!focused_fullscreen_on(Some(&foreground), monitor, work));
        assert!(!focused_fullscreen_on(None, monitor, work));
    }

    #[test]
    fn focused_fullscreen_blocks_its_monitor_and_not_the_other_display() {
        let left = RECT {
            left: -1920,
            top: -120,
            right: 0,
            bottom: 960,
        };
        let right = RECT {
            left: 0,
            top: 0,
            right: 2560,
            bottom: 1440,
        };
        let left_work = RECT { bottom: 920, ..left };
        let right_work = RECT { bottom: 1400, ..right };
        let focused_left = surface(left, false);
        assert!(focused_fullscreen_on(Some(&focused_left), left, left_work));
        assert!(!focused_fullscreen_on(Some(&focused_left), right, right_work));
        let focused_right = surface(right, false);
        assert!(!focused_fullscreen_on(Some(&focused_right), left, left_work));
        assert!(focused_fullscreen_on(Some(&focused_right), right, right_work));
        let crossing = surface(
            RECT {
                left: -500,
                top: 100,
                right: 500,
                bottom: 900,
            },
            false,
        );
        assert!(!focused_fullscreen_on(Some(&crossing), left, left_work));
        assert!(!focused_fullscreen_on(Some(&crossing), right, right_work));
    }

    #[test]
    fn normal_maximized_work_area_does_not_count_as_focused_fullscreen() {
        let monitor = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let work = RECT {
            bottom: 1040,
            ..monitor
        };
        assert!(!focused_fullscreen_on(Some(&surface(work, true)), monitor, work));
        assert!(!focused_fullscreen_on(Some(&surface(monitor, true)), monitor, monitor));
        let framed = SurfaceWindow {
            bounds: monitor,
            raw: RECT {
                left: -8,
                top: -8,
                right: 1928,
                bottom: 1088,
            },
            maximized: true,
            borderless: false,
            monitor: HMONITOR::default(),
        };
        assert!(!focused_fullscreen_on(Some(&framed), monitor, work));
        assert!(focused_fullscreen_on(Some(&surface(monitor, false)), monitor, work));
    }

    #[test]
    fn borderless_fullscreen_survives_work_area_reset_on_its_own_display() {
        let monitor = RECT {
            left: -1920,
            top: -120,
            right: 0,
            bottom: 960,
        };
        let work = RECT {
            top: -96,
            bottom: 920,
            ..monitor
        };
        let other = RECT {
            left: 0,
            top: 0,
            right: 2560,
            bottom: 1440,
        };
        let fullscreen = SurfaceWindow {
            borderless: true,
            ..surface(monitor, true)
        };
        for current_work in [work, monitor, work] {
            assert!(focused_fullscreen_on(Some(&fullscreen), monitor, current_work));
            assert!(!focused_fullscreen_on(Some(&fullscreen), other, other));
        }
        assert!(!focused_fullscreen_on(None, monitor, monitor));
        assert!(!focused_fullscreen_on(Some(&surface(monitor, true)), monitor, monitor));
        let ordinary = SurfaceWindow {
            borderless: true,
            ..surface(work, true)
        };
        assert!(!focused_fullscreen_on(Some(&ordinary), monitor, monitor));
        let framed_raw = SurfaceWindow {
            raw: RECT {
                left: monitor.left - 8,
                right: monitor.right + 8,
                ..monitor
            },
            ..fullscreen
        };
        assert!(!focused_fullscreen_on(Some(&framed_raw), monitor, monitor));
    }

    #[test]
    fn captioned_or_resizable_custom_chrome_is_not_borderless_fullscreen() {
        assert!(borderless_surface(WS_POPUP.0 | WS_VISIBLE.0));
        for frame in [WS_CAPTION.0, WS_BORDER.0, WS_DLGFRAME.0, WS_THICKFRAME.0] {
            assert!(!borderless_surface(WS_POPUP.0 | WS_VISIBLE.0 | frame));
        }
    }

    #[test]
    fn desktop_shell_and_own_windows_are_excluded_before_fullscreen_selection() {
        for class in ["Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd"] {
            assert!(excluded_surface(101, 202, &class.encode_utf16().collect::<Vec<_>>()));
        }
        assert!(excluded_surface(
            202,
            202,
            &"Wisp.TopBar".encode_utf16().collect::<Vec<_>>()
        ));
        assert!(!excluded_surface(
            101,
            202,
            &"ApplicationFrameWindow".encode_utf16().collect::<Vec<_>>()
        ));
    }

    #[test]
    fn fullscreen_blocks_only_its_monitor_even_if_another_monitor_has_focus() {
        let left = RECT {
            left: -1920,
            top: 0,
            right: 0,
            bottom: 1080,
        };
        let right = RECT {
            left: 0,
            top: 0,
            right: 2560,
            bottom: 1440,
        };
        let right_work = RECT { bottom: 1400, ..right };
        let windows = [
            SurfaceWindow {
                bounds: right_work,
                raw: right_work,
                maximized: true,
                borderless: false,
                monitor: HMONITOR::default(),
            },
            SurfaceWindow {
                bounds: left,
                raw: left,
                maximized: false,
                borderless: true,
                monitor: HMONITOR::default(),
            },
        ];
        assert!(fullscreen_on(&windows, left, left));
        assert!(!fullscreen_on(&windows, right, right_work));
    }
}

/// Shares a z-ordered snapshot of visible windows across the docks.
unsafe extern "system" fn collect_surface(hwnd: HWND, param: LPARAM) -> BOOL {
    if let Some(window) = unsafe { read_surface(hwnd) } {
        unsafe {
            (*(param.0 as *mut Vec<SurfaceWindow>)).push(window);
        }
    }
    true.into()
}

/// Existing surface eligibility and bounds, shared with the fresh focused read.
unsafe fn read_surface(hwnd: HWND) -> Option<SurfaceWindow> {
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
    unsafe {
        if !IsWindowVisible(hwnd).as_bool()
            || IsIconic(hwnd).as_bool()
            || GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0
        {
            return None;
        }
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let mut class = [0u16; 64];
        let len = GetClassNameW(hwnd, &mut class) as usize;
        if excluded_surface(pid, std::process::id(), &class[..len]) {
            return None;
        }
        let mut raw = RECT::default();
        if GetWindowRect(hwnd, &mut raw).is_err() {
            return None;
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&raw mut cloaked).cast(), 4);
        if cloaked != 0 {
            return None;
        }
        let mut bounds = RECT::default();
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&raw mut bounds).cast(),
            size_of::<RECT>() as u32,
        )
        .is_err()
        {
            bounds = raw;
        }
        Some(SurfaceWindow {
            bounds,
            raw,
            maximized: IsZoomed(hwnd).as_bool(),
            borderless: borderless_surface(GetWindowLongW(hwnd, GWL_STYLE) as u32),
            monitor: MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL),
        })
    }
}

unsafe extern "system" fn window_event(
    _: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    object: i32,
    child: i32,
    _: u32,
    _: u32,
) {
    if event != EVENT_SYSTEM_FOREGROUND {
        if !matches!(
            event,
            EVENT_OBJECT_CREATE
                | EVENT_OBJECT_DESTROY
                | EVENT_OBJECT_SHOW
                | EVENT_OBJECT_HIDE
                | EVENT_OBJECT_NAMECHANGE
                | EVENT_OBJECT_LOCATIONCHANGE
                | EVENT_OBJECT_CLOAKED
                | EVENT_OBJECT_UNCLOAKED
        ) || object != OBJID_WINDOW.0
            || child != 0
            || hwnd.is_invalid()
        {
            return;
        }
        if event != EVENT_OBJECT_DESTROY && unsafe { GetAncestor(hwnd, GA_ROOT) } != hwnd {
            return;
        }
    }
    post_to_docks(WM_REFRESH, None);
}

fn hide_popup_host_before_message(message: u32, timer: usize) -> bool {
    (message == WM_TIMER && matches!(timer, REFRESH_TIMER | SLIDE_TIMER))
        || matches!(
            message,
            WM_DPICHANGED
                | WM_DISPLAYCHANGE
                | WM_SETTINGCHANGE
                | WM_THEMECHANGED
                | WM_POWERBROADCAST
                | WM_SHOWWINDOW
                | WM_WINDOWPOSCHANGING
                | WM_DESTROY
                | WM_NCDESTROY
                | WM_LBUTTONDOWN
                | WM_LBUTTONUP
                | WM_RBUTTONUP
                | WM_CLOSE_FLYOUT
                | WM_SURFACE_MATERIAL
        )
}
#[cfg(test)]
mod material_message_tests {
    use super::*;
    #[test]
    fn panel_update_boundary_keeps_reentrant_status_and_owner_removal() {
        struct Owner {
            alive: bool,
            epoch: u64,
            material: u64,
            status: u32,
        }
        let owner = RefCell::new(Owner {
            alive: true,
            epoch: 7,
            material: 2,
            status: 1,
        });
        unborrowed_action(
            &owner,
            |state| (state.epoch, state.material),
            |snapshot| {
                // Models host preparation/geometry pumping a fresh status callback
                // and removing its owning Dock or Bar while this effect is active.
                let mut state = owner.borrow_mut();
                state.status = 9;
                state.alive = false;
                assert!(!panel_generation_current(
                    snapshot,
                    (state.epoch, state.material),
                    true,
                    state.alive
                ));
            },
        );
        assert_eq!(owner.borrow().status, 9);
        assert!(!panel_generation_current((7, 2), (8, 2), true, true));
        assert!(!panel_generation_current((7, 2), (7, 3), true, true));
        assert!(!panel_generation_current((7, 2), (7, 2), false, true));
    }

    #[test]
    fn polling_and_cached_fades_keep_the_retained_host_visible() {
        assert!(!hide_popup_host_before_message(WM_TIMER, POLL_TIMER));
        assert!(!hide_popup_host_before_message(WM_TIMER, POPUP_FADE_TIMER));
        assert!(!hide_popup_host_before_message(WM_MOUSEMOVE, 0));
        assert!(hide_popup_host_before_message(WM_TIMER, SLIDE_TIMER));
        assert!(hide_popup_host_before_message(WM_CLOSE_FLYOUT, 0));
        assert!(hide_popup_host_before_message(WM_SURFACE_MATERIAL, 0));
    }
}

// Companion effects run after the foreground procedure has released State.
unsafe fn surface_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    inner: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
) -> LRESULT {
    unsafe {
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<State>;
        if pointer.is_null() {
            return inner(hwnd, msg, wp, lp);
        }
        Rc::increment_strong_count(pointer);
        let state = Rc::from_raw(pointer);
        let snapshot = state
            .try_borrow_mut()
            .ok()
            .map(|mut s| (s.material_version, s.flyout_material.take()));
        let Some((version, mut material)) = snapshot else {
            return inner(hwnd, msg, wp, lp);
        };
        if hide_popup_host_before_message(msg, wp.0)
            && let Some(host) = &material
        {
            host.hide();
        }
        let result = inner(hwnd, msg, wp, lp);
        let current = {
            let s = state.borrow();
            crate::theme::material_is_current(
                version,
                s.material_version,
                s.alive,
                GetWindowLongPtrW(hwnd, GWLP_USERDATA) == pointer as isize,
            )
        };
        if msg == WM_NCDESTROY || !current {
            return result;
        }
        if msg == WM_SURFACE_MATERIAL {
            drop(material.take());
            let (owner, flyout, dark, backdrop, color, remote, options) = {
                let s = state.borrow();
                (
                    s.owner,
                    s.flyout,
                    s.dark,
                    s.backdrop,
                    s.backdrop_color.clone(),
                    s.remote,
                    s.material_options,
                )
            };
            if remote {
                material = Some(crate::theme::SurfaceMaterial::prepare(
                    Some(owner),
                    flyout,
                    dark,
                    backdrop,
                    &color,
                    options,
                ));
            }
            if GetWindowLongPtrW(hwnd, GWLP_USERDATA) != pointer as isize
                || GetWindowLongPtrW(flyout, GWLP_USERDATA) != pointer as isize
            {
                return result;
            }
            let mut s = state.borrow_mut();
            if !s.alive || s.material_version != version {
                return result;
            }
            if let Some(material) = &material {
                s.flyout_palette = material.palette.clone();
            }
            s.flyout_prepared = false;
            s.flyout_renderer.discard_popup_surface();
            let _ = InvalidateRect(Some(flyout), None, false);
            let panel = s.sound_identity.map(|id| PanelTarget {
                id,
                epoch: s.sound_epoch,
                material_version: s.material_version,
            });
            drop(s);
            if let Some(target) = panel.filter(|target| dock_panel_current(&state, *target)) {
                sound::apply_theme(target.id.hwnd, dark, backdrop, &color, options);
            }
        }
        let (current, alive, flyout, ready, dpi, alpha) = {
            let s = state.borrow();
            (
                s.material_version,
                s.alive,
                s.flyout,
                s.remote && s.flyout_open && s.flyout_prepared,
                s.dpi,
                s.flyout_exit.or(s.remote_flyout_fade).map_or(255, PopupFade::alpha),
            )
        };
        if !crate::theme::material_is_current(
            version,
            current,
            alive,
            GetWindowLongPtrW(hwnd, GWLP_USERDATA) == pointer as isize
                && GetWindowLongPtrW(flyout, GWLP_USERDATA) == pointer as isize,
        ) {
            return result;
        }
        let mut repaint = false;
        if let Some(host) = &mut material {
            let mut bounds = RECT::default();
            if ready && IsWindowVisible(flyout).as_bool() && GetWindowRect(flyout, &mut bounds).is_ok() {
                repaint = !host.present(bounds, dpi, 8., alpha as f32 / 255.);
            } else {
                host.hide();
            }
        }
        if GetWindowLongPtrW(hwnd, GWLP_USERDATA) != pointer as isize
            || GetWindowLongPtrW(flyout, GWLP_USERDATA) != pointer as isize
        {
            return result;
        }
        let mut s = state.borrow_mut();
        if s.alive && s.material_version == version {
            if let Some(host) = &material {
                s.flyout_palette = host.palette.clone();
            }
            s.flyout_material = material;
            if repaint {
                s.flyout_prepared = false;
                s.flyout_renderer.discard_popup_surface();
                let _ = InvalidateRect(Some(flyout), None, false);
            }
        }
        result
    }
}
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { surface_proc(hwnd, msg, wp, lp, wndproc_inner) }
}
unsafe extern "system" fn popup_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { surface_proc(hwnd, msg, wp, lp, popup_wndproc_inner) }
}

unsafe extern "system" fn wndproc_inner(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let create = &*(lp.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        if msg == WM_NCACTIVATE {
            return DefWindowProcW(hwnd, msg, WPARAM(1), LPARAM(-1));
        }
        if msg == WM_MOUSEACTIVATE {
            return LRESULT(MA_NOACTIVATE as isize);
        }
        if msg == WM_NCCALCSIZE && wp.0 != 0 {
            return LRESULT(0);
        }
        if msg == WM_NCHITTEST {
            return LRESULT(HTCLIENT as isize);
        }
        if msg == WM_ERASEBKGND {
            return LRESULT(1);
        }
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<State>;
        if !pointer.is_null()
            && let Ok(mut s) = (*pointer).try_borrow_mut()
        {
            match msg {
                WM_SETTINGCHANGE | WM_POWERBROADCAST | WM_THEMECHANGED => {
                    let dark = s.palette.text.r > 0.5;
                    let backdrop = s.backdrop;
                    let color = s.backdrop_color.clone();
                    let options = s.material_options;
                    s.apply_material(dark, backdrop, &color, options);
                }
                WM_REFRESH => {
                    if !s.scheduled {
                        s.scheduled = SetTimer(Some(hwnd), REFRESH_TIMER, 100, None) != 0;
                    }
                    return LRESULT(0);
                }
                WM_TIMER if wp.0 == POLL_TIMER => {
                    s.poll();
                    return LRESULT(0);
                }
                WM_TIMER if wp.0 == SLIDE_TIMER => {
                    s.step_slide();
                    return LRESULT(0);
                }
                WM_TIMER if wp.0 == HOVER_TIMER => {
                    s.step_hover();
                    return LRESULT(0);
                }
                WM_TIMER if wp.0 == TIP_TIMER => {
                    s.show_tip();
                    return LRESULT(0);
                }
                WM_TIMER if wp.0 == DESKTOP_TIMER => {
                    let _ = KillTimer(Some(hwnd), DESKTOP_TIMER);
                    if s.desktop_pill.take().is_some() {
                        let _ = ShowWindow(s.tip, SW_HIDE);
                    }
                    return LRESULT(0);
                }
                WM_TIMER if wp.0 == WINDOWS_TIMER => {
                    s.show_windows();
                    return LRESULT(0);
                }
                WM_TIMER if wp.0 == PREVIEW_ZOOM_TIMER => {
                    s.show_zoom();
                    return LRESULT(0);
                }
                WM_TIMER if wp.0 == WINDOWS_HIDE_TIMER => {
                    let _ = KillTimer(Some(hwnd), WINDOWS_HIDE_TIMER);
                    let mut cursor = POINT::default();
                    let _ = GetCursorPos(&mut cursor);
                    if !s.over_preview(cursor) {
                        s.hide_windows();
                    } else {
                        s.leave_windows();
                    }
                    return LRESULT(0);
                }
                WM_TRAY_CHANGED | WM_DOCK_TRAY_SYNC => {
                    s.sync_tray();
                    if s.primary && msg == WM_TRAY_CHANGED {
                        post_to_docks(WM_DOCK_TRAY_SYNC, Some(hwnd));
                    }
                    return LRESULT(0);
                }
                WM_TIMER if wp.0 == REFRESH_TIMER => {
                    let _ = KillTimer(Some(hwnd), REFRESH_TIMER);
                    s.scheduled = false;
                    s.refresh();
                    return LRESULT(0);
                }
                WM_DPICHANGED | WM_DISPLAYCHANGE => {
                    s.hide_windows();
                    SURFACE_CACHE.with_borrow_mut(|cache| *cache = None);
                    if !s.revealed {
                        s.place();
                    }
                    let _ = PostMessageW(Some(hwnd), WM_REFRESH, WPARAM(0), LPARAM(0));
                    return LRESULT(0);
                }
                WM_LBUTTONDOWN => {
                    if let Some(item) = s.hit(lp).and_then(|slot| s.item_at(slot)) {
                        s.app_press = Some(AppPress {
                            id: item.app.id.clone(),
                            start: mouse_point(lp),
                            pointer: Some(lp),
                            dragging: false,
                        });
                        let _ = SetCapture(hwnd);
                    }
                    return LRESULT(0);
                }
                WM_CAPTURECHANGED | WM_CANCELMODE => {
                    s.cancel_app_press();
                    return LRESULT(0);
                }
                WM_MOUSEMOVE => {
                    let mut hover = s.hit(lp);
                    let threshold = (6 * s.dpi / 96) as i32;
                    if let Some(press) = &mut s.app_press {
                        let point = mouse_point(lp);
                        press.pointer = Some(lp);
                        if !press.dragging
                            && (point.x - press.start.x).abs().max((point.y - press.start.y).abs()) >= threshold
                        {
                            press.dragging = true;
                            s.hide_windows();
                            s.hide_tip();
                        }
                    }
                    let dragging = s.app_press.as_ref().is_some_and(|press| press.dragging);
                    let mut hover_part = s.hit_part(lp);
                    if dragging {
                        hover = s.drag_frame().map(|drag| drag.from);
                        hover_part = None;
                    }
                    if s.hover != hover || s.hover_part != hover_part {
                        s.hover = hover;
                        s.animate_hover();
                        s.hover_part = hover_part;
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    if dragging {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                        return LRESULT(0);
                    }
                    s.hover_tip(match hover_part {
                        Some(logic::Part::Chevron) => Some(tray::Tip::Chevron),
                        Some(logic::Part::Tray(i)) => Some(tray::Tip::Inline(i)),
                        _ => None,
                    });
                    s.hover_windows(if hover_part.is_none() { hover } else { None });
                    let _ = TrackMouseEvent(&mut TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    });
                    return LRESULT(0);
                }
                WM_MOUSELEAVE => {
                    if let Some(press) = &mut s.app_press {
                        press.pointer = None;
                    }
                    s.hover = None;
                    s.animate_hover();
                    s.hover_part = None;
                    s.hover_tip(None);
                    s.leave_windows();
                    let _ = InvalidateRect(Some(hwnd), None, false);
                    return LRESULT(0);
                }
                WM_LBUTTONUP | WM_MBUTTONUP => {
                    if msg == WM_LBUTTONUP
                        && let Some(press) = s.app_press.take()
                    {
                        let _ = ReleaseCapture();
                        let target = s
                            .hit(lp)
                            .and_then(|slot| s.item_at(slot))
                            .map(|item| item.app.id.clone());
                        if press.dragging {
                            if let Some((_, item)) = s.drop_target(&press.id, lp) {
                                let target = item.app.id.clone();
                                s.reorder_app(&press.id, &target);
                            }
                            s.hover = s.hit(lp);
                            s.animate_hover();
                            let _ = InvalidateRect(Some(hwnd), None, false);
                            return LRESULT(0);
                        }
                        if !target.is_some_and(|id| id.eq_ignore_ascii_case(&press.id)) {
                            return LRESULT(0);
                        }
                    }
                    // The dock never takes the focus, so the flyout can't see this click.
                    if s.hit_part(lp) != Some(logic::Part::Chevron) {
                        s.close_flyout();
                    }
                    if let Some(index) = s.hit(lp) {
                        if msg == WM_MBUTTONUP
                            || !s.item_at(index).is_some_and(|item| s.preview_windows(item).len() > 1)
                        {
                            s.hide_windows();
                        }
                        let owned = retain_dock_state(pointer);
                        drop(s);
                        run_dock_click(&owned, index, msg == WM_MBUTTONUP);
                        return LRESULT(0);
                    } else if let Some(part) = s.hit_part(lp)
                        && msg == WM_LBUTTONUP
                    {
                        s.hide_windows();
                        let effect = s.click_part(part);
                        let owned = retain_dock_state(pointer);
                        drop(s);
                        run_dock_effect(&owned, effect);
                        return LRESULT(0);
                    } else {
                        s.hide_windows();
                    }
                    return LRESULT(0);
                }
                WM_RBUTTONUP => {
                    s.close_flyout();
                    s.hide_windows();
                    if let Some(index) = s.hit(lp) {
                        let owned = retain_dock_state(pointer);
                        drop(s);
                        run_dock_menu(&owned, Some(index));
                    } else {
                        match s.hit_part(lp) {
                            Some(logic::Part::Volume) => {
                                let owned = retain_dock_state(pointer);
                                drop(s);
                                run_dock_effect(&owned, DockEffect::Controls);
                            }
                            Some(logic::Part::Chevron) => {
                                let owned = retain_dock_state(pointer);
                                drop(s);
                                run_dock_menu(&owned, None);
                            }
                            Some(logic::Part::Tray(position)) => s.click_inline(position, Action::Menu),
                            _ => {}
                        }
                    }
                    return LRESULT(0);
                }
                WM_MOUSEWHEEL => {
                    s.hide_windows();
                    let delta = (wp.0 >> 16) as i16;
                    // Wheel messages carry screen coordinates.
                    let mut point = POINT {
                        x: (lp.0 as i16).into(),
                        y: ((lp.0 >> 16) as i16).into(),
                    };
                    let _ = ScreenToClient(hwnd, &mut point);
                    let client = LPARAM(((point.y as u16 as isize) << 16) | point.x as u16 as isize);
                    if s.hit_part(client) == Some(logic::Part::Volume) {
                        s.wheel_volume(delta.into());
                        return LRESULT(0);
                    }
                    // Shift scrolls the apps when more run than fit.
                    if GetKeyState(VK_SHIFT.0 as i32) >= 0 {
                        s.wheel_desktops(delta.into());
                        return LRESULT(0);
                    }
                    s.scroll = if delta < 0 {
                        (s.scroll + 1).min(s.items.len().saturating_sub(s.capacity))
                    } else {
                        s.scroll.saturating_sub(1)
                    };
                    let _ = InvalidateRect(Some(hwnd), None, false);
                    return LRESULT(0);
                }
                WM_MOUSEHWHEEL => {
                    s.hide_windows();
                    // Positive is a swipe toward the right.
                    s.wheel_desktops(-i32::from((wp.0 >> 16) as i16));
                    return LRESULT(0);
                }
                WM_PAINT => {
                    let mut paint = PAINTSTRUCT::default();
                    let _ = BeginPaint(hwnd, &mut paint);
                    if let Err(error) = s.paint(None) {
                        eprintln!("Dock paint: {error}");
                    }
                    let _ = EndPaint(hwnd, &paint);
                    return LRESULT(0);
                }
                WM_CLOSE => {
                    s.hide_windows();
                    s.stop_slide();
                    s.visible = false;
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    return LRESULT(0);
                }
                WM_NCDESTROY => {
                    s.alive = false;
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                _ => {}
            }
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

/// The flyout and the tooltip, which share the dock's state.
unsafe extern "system" fn popup_wndproc_inner(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let create = &*(lp.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        if msg == WM_NCCALCSIZE && wp.0 != 0 {
            return LRESULT(0);
        }
        if msg == WM_ERASEBKGND {
            return LRESULT(1);
        }
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<State>;
        let Some(cell) = pointer.as_ref() else {
            return DefWindowProcW(hwnd, msg, wp, lp);
        };
        let zoom = cell.try_borrow().is_ok_and(|s| s.zoom == hwnd);
        if zoom {
            match msg {
                WM_NCHITTEST => return LRESULT(HTTRANSPARENT as isize),
                WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
                WM_PAINT => {
                    if let Ok(mut s) = cell.try_borrow_mut() {
                        let mut paint = PAINTSTRUCT::default();
                        let _ = BeginPaint(hwnd, &mut paint);
                        let State {
                            zoom_renderer,
                            dpi,
                            zoom_palette: palette,
                            zoom_title,
                            ..
                        } = &mut *s;
                        let _ = zoom_renderer.draw_window_zoom(hwnd, *dpi, palette, zoom_title);
                        let _ = EndPaint(hwnd, &paint);
                    }
                    return LRESULT(0);
                }
                _ => return DefWindowProcW(hwnd, msg, wp, lp),
            }
        }
        let tip = cell.try_borrow().is_ok_and(|s| s.tip == hwnd);
        if tip {
            // The tooltip never takes the pointer or the focus.
            match msg {
                WM_NCHITTEST => return LRESULT(HTTRANSPARENT as isize),
                WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
                _ => {}
            }
        } else if msg == WM_NCHITTEST {
            return LRESULT(HTCLIENT as isize);
        }
        let windows = cell.try_borrow().is_ok_and(|s| s.windows == hwnd);
        if windows {
            match msg {
                // Clicks land here, but the list never takes the focus.
                WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
                WM_MOUSEMOVE => {
                    let Ok(mut s) = cell.try_borrow_mut() else {
                        return DefWindowProcW(hwnd, msg, wp, lp);
                    };
                    s.away = 0;
                    let _ = KillTimer(Some(s.hwnd), WINDOWS_HIDE_TIMER);
                    let hover = s.windows_hit(lp);
                    if s.windows_hover != hover {
                        s.hide_zoom();
                        s.windows_hover = hover;
                        if hover.is_some_and(|(_, close)| !close) {
                            let _ = SetTimer(Some(s.hwnd), PREVIEW_ZOOM_TIMER, 180, None);
                        }
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    let _ = TrackMouseEvent(&mut TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    });
                    return LRESULT(0);
                }
                WM_MOUSELEAVE => {
                    let Ok(mut s) = cell.try_borrow_mut() else {
                        return DefWindowProcW(hwnd, msg, wp, lp);
                    };
                    s.hide_zoom();
                    s.windows_hover = None;
                    s.leave_windows();
                    return LRESULT(0);
                }
                WM_MOUSEWHEEL => {
                    if let Ok(mut s) = cell.try_borrow_mut() {
                        s.change_preview_page(((wp.0 >> 16) as i16) < 0);
                    }
                    return LRESULT(0);
                }
                WM_LBUTTONDOWN | WM_MBUTTONDOWN => {
                    if let Ok(mut s) = cell.try_borrow_mut() {
                        s.hide_zoom();
                        s.windows_press = s.preview_press(lp, msg == WM_MBUTTONDOWN);
                        let _ = SetCapture(hwnd);
                    }
                    return LRESULT(0);
                }
                WM_CANCELMODE | WM_CAPTURECHANGED => {
                    if let Ok(mut s) = cell.try_borrow_mut() {
                        s.windows_press = None;
                    }
                    return LRESULT(0);
                }
                WM_LBUTTONUP | WM_MBUTTONUP => {
                    let Ok(mut s) = cell.try_borrow_mut() else {
                        return DefWindowProcW(hwnd, msg, wp, lp);
                    };
                    let pressed = s.windows_press.take();
                    if GetCapture() == hwnd {
                        let _ = ReleaseCapture();
                    }
                    let scale = s.dpi as f32 / 96.;
                    let y = ((lp.0 >> 16) as i16) as f32 / scale;
                    if msg == WM_LBUTTONUP
                        && s.preview_count > 1
                        && y >= logic::PREVIEW_PAD
                            + logic::PREVIEW_HEADER
                            + s.preview_height
                            + s.monitor_caption_height()
                    {
                        let x = (lp.0 as i16) as f32 / scale;
                        let mut client = RECT::default();
                        if GetClientRect(hwnd, &mut client).is_ok() {
                            let width = client.right as f32 / scale;
                            if x < logic::PREVIEW_PAD + 36. || x > width - logic::PREVIEW_PAD - 36. {
                                s.change_preview_page(x > width / 2.);
                            }
                        }
                        return LRESULT(0);
                    }
                    if let Some(pressed) = pressed
                        && let Some(current) = s.preview_press(lp, msg == WM_MBUTTONUP)
                        && let Some((target, close)) = pressed.release(current)
                    {
                        if !close {
                            s.hide_windows();
                        }
                        let effect = if close {
                            DockEffect::Close(vec![(target, pressed.pid)])
                        } else {
                            DockEffect::Focus((target, pressed.pid))
                        };
                        let owned = retain_dock_state(pointer);
                        drop(s);
                        run_dock_effect(&owned, effect);
                        return LRESULT(0);
                    }
                    return LRESULT(0);
                }
                WM_PAINT => {
                    let Ok(mut s) = cell.try_borrow_mut() else {
                        return DefWindowProcW(hwnd, msg, wp, lp);
                    };
                    let mut paint = PAINTSTRUCT::default();
                    let _ = BeginPaint(hwnd, &mut paint);
                    let monitor_height = s.monitor_caption_height();
                    let State {
                        windows_renderer,
                        windows_palette: palette,
                        dpi,
                        windows_rows,
                        windows_monitors,
                        windows_hover,
                        preview_height,
                        preview_width,
                        previews,
                        preview_page,
                        preview_count,
                        ..
                    } = &mut *s;
                    if let Err(error) = windows_renderer.draw_windows(
                        hwnd,
                        *dpi,
                        palette,
                        windows_rows,
                        (windows_monitors, monitor_height),
                        *windows_hover,
                        (*preview_width, *preview_height),
                        previews,
                        (*preview_page, *preview_count),
                    ) {
                        eprintln!("Dock window list: {error}");
                    }
                    let _ = EndPaint(hwnd, &paint);
                    return LRESULT(0);
                }
                _ => return DefWindowProcW(hwnd, msg, wp, lp),
            }
        }
        let Ok(mut s) = cell.try_borrow_mut() else {
            return DefWindowProcW(hwnd, msg, wp, lp);
        };
        let scale = s.dpi as f32 / 96.0;
        let cell_at = |s: &State, lp: LPARAM| {
            logic::flyout_hit(
                (lp.0 as i16) as f32 / scale,
                ((lp.0 >> 16) as i16) as f32 / scale,
                s.tray.len(),
            )
        };
        match msg {
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut paint);
                if s.remote && !tip {
                    if let Err(error) = s.present_remote_flyout(false) {
                        crate::logging::event("tray.remote.paint.failed", format_args!("error={error}"));
                        s.close_flyout();
                    }
                    let _ = EndPaint(hwnd, &paint);
                    return LRESULT(0);
                }
                let State {
                    tray,
                    flyout_palette,
                    tip_palette,
                    dpi,
                    flyout_hover,
                    flyout_renderer,
                    tip_renderer,
                    tip_text,
                    desktop_pill,
                    ..
                } = &mut *s;
                let palette = if tip { tip_palette } else { flyout_palette };
                let result = if let Some(pill) = desktop_pill.as_ref().filter(|_| tip) {
                    tip_renderer.draw_desktops(
                        hwnd,
                        *dpi,
                        palette,
                        (pill.current, pill.names.len()),
                        &pill.names[pill.current],
                    )
                } else if tip {
                    tip_renderer.draw_tip(hwnd, *dpi, palette, tip_text)
                } else {
                    flyout_renderer.draw_flyout(
                        hwnd,
                        &render::FlyoutFrame {
                            dpi: *dpi,
                            palette,
                            tray,
                            hover: *flyout_hover,
                        },
                    )
                };
                if let Err(error) = result {
                    eprintln!("Dock popup paint: {error}");
                }
                let _ = EndPaint(hwnd, &paint);
                return LRESULT(0);
            }
            _ if tip => {}
            WM_TIMER if wp.0 == POPUP_FADE_TIMER => {
                if let Some(exit) = s.flyout_exit {
                    if exit.finished() {
                        s.close_flyout();
                    } else if s.remote {
                        if let Err(error) = s.present_remote_flyout(true) {
                            crate::logging::event("tray.remote.close.failed", format_args!("error={error}"));
                            s.close_flyout();
                        }
                    } else if SetLayeredWindowAttributes(hwnd, COLORREF(0), exit.alpha(), LWA_ALPHA).is_err() {
                        s.close_flyout();
                    }
                    return LRESULT(0);
                }
                if s.remote {
                    if s.remote_flyout_fade.is_some_and(RemotePopupFade::finished) {
                        s.remote_flyout_fade = None;
                        let _ = KillTimer(Some(hwnd), POPUP_FADE_TIMER);
                    }
                    if s.flyout_open
                        && let Err(error) = s.present_remote_flyout(true)
                    {
                        crate::logging::event("tray.remote.present.failed", format_args!("error={error}"));
                        s.close_flyout();
                    }
                } else {
                    tick_popup_fade(hwnd, &mut s.flyout_fade);
                }
                return LRESULT(0);
            }
            WM_MOUSEMOVE => {
                let hover = cell_at(&s, lp);
                if s.flyout_hover != hover {
                    s.flyout_hover = hover;
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                s.hover_tip(hover.map(tray::Tip::Flyout));
                let _ = TrackMouseEvent(&mut TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                });
                return LRESULT(0);
            }
            WM_MOUSELEAVE => {
                s.flyout_hover = None;
                s.hover_tip(None);
                let _ = InvalidateRect(Some(hwnd), None, false);
                return LRESULT(0);
            }
            // Leave the overflow visible until the app takes focus or the user dismisses it.
            WM_LBUTTONUP | WM_RBUTTONUP => {
                if let Some(index) = cell_at(&s, lp) {
                    let rect = s.cell_rect(index);
                    let center = POINT {
                        x: (rect.left + rect.right) / 2,
                        y: (rect.top + rect.bottom) / 2,
                    };
                    let action = if msg == WM_LBUTTONUP {
                        Action::Click
                    } else {
                        Action::Menu
                    };
                    s.click_tray(index, action, center);
                }
                return LRESULT(0);
            }
            WM_KEYDOWN if wp.0 == VK_ESCAPE.0 as usize => {
                s.dismiss_flyout();
                return LRESULT(0);
            }
            // Closing within activation changes can reenter; close afterwards.
            WM_ACTIVATE if (wp.0 & 0xFFFF) as u32 == WA_INACTIVE => {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE_FLYOUT, WPARAM(0), LPARAM(0));
            }
            WM_CLOSE_FLYOUT => {
                // A held demo flyout stays for its screenshot.
                if !s.held && !s.tray_menu_active() && GetForegroundWindow() != hwnd {
                    s.dismiss_flyout();
                }
                return LRESULT(0);
            }
            WM_CLOSE => {
                s.dismiss_flyout();
                return LRESULT(0);
            }
            _ => {}
        }
        drop(s);
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}
