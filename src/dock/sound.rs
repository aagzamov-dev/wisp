//! Dock-owned Control Center. Windows queries and mutations stay on the command worker.

use std::{cell::RefCell, collections::VecDeque, rc::Rc, time::Instant};

use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Input::KeyboardAndMouse::{
                GetAsyncKeyState, ReleaseCapture, SetCapture, VK_ESCAPE, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON,
                VK_XBUTTON1, VK_XBUTTON2,
            },
            WindowsAndMessaging::*,
        },
    },
    core::{Error, Result, w},
};

use super::render::Renderer;
use crate::commands::{Command, State, control, glyph, media};
use crate::theme::Palette;

pub const WM_SOUND_ACTION: u32 = WM_APP + 26;
pub const WM_SOUND_REFRESH: u32 = WM_APP + 27;
const REFRESH: usize = 1;
const PAGE_FADE: usize = 3;
const SURFACE_MOTION: usize = 4;
const INPUT_ESCAPE: u32 = WM_APP + 48;
const DISMISS: u32 = WM_APP + 47;
const IDENTITY: windows::core::PCWSTR = w!("Wisp.ControlCenter.Generation");
static NEXT_IDENTITY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub hwnd: HWND,
    serial: usize,
}
pub fn identity(hwnd: HWND) -> Option<Identity> {
    let serial = unsafe { GetPropW(hwnd, IDENTITY) }.0 as usize;
    (serial != 0).then_some(Identity { hwnd, serial })
}
fn same_identity(expected: usize, actual: usize) -> bool {
    expected != 0 && expected == actual
}
pub fn is_current(id: Identity) -> bool {
    same_identity(id.serial, unsafe { GetPropW(id.hwnd, IDENTITY) }.0 as usize)
}
pub fn destroy(id: Identity) {
    if is_current(id) {
        material_effect(id.hwnd, false);
        if is_current(id) {
            let _ = unsafe { DestroyWindow(id.hwnd) };
        }
    }
}
/// Owners may hold their own RefCell guard. Teardown is dispatched later.
pub fn defer_destroy(id: Identity) {
    post_close(id, true);
}
fn post_close(id: Identity, immediate: bool) {
    if is_current(id)
        && let Err(error) = unsafe {
            PostMessageW(
                Some(id.hwnd),
                DISMISS,
                WPARAM(id.serial),
                LPARAM(isize::from(immediate)),
            )
        }
    {
        crate::logging::event(
            "controls.close.failed",
            format_args!("serial={} error={error}", id.serial),
        );
    }
}
/// Layout changes move the trigger independently of the open panel.
pub fn update_opener(id: Identity, anchor: RECT) {
    if is_current(id)
        && let Some(cell) = panel(id.hwnd)
        && let Ok(mut p) = cell.try_borrow_mut()
    {
        p.opener = anchor;
        update_input(id, |target| target.opener = anchor);
    }
}
fn request_close(hwnd: HWND, immediate: bool) {
    if let Some(id) = identity(hwnd) {
        post_close(id, immediate);
    }
}

/// Record transitions, including complete presses between message dispatches.
/// Initial held state suppresses only the gesture which opened the panel.
#[derive(Clone, Copy, Default)]
struct InputGuard {
    buttons: u8,
    escape: bool,
}
impl InputGuard {
    fn button(&mut self, bit: u8, down: bool, inside: bool, opener: bool) -> bool {
        let pressed = self.buttons & bit != 0;
        if down {
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
        }
        down && !pressed && !inside && !opener
    }
    fn escape(&mut self, down: bool) -> bool {
        let pressed = down && !self.escape;
        self.escape = down;
        pressed
    }
}
#[derive(Clone, Copy)]
struct InputTarget {
    id: Identity,
    bounds: RECT,
    opener: RECT,
    focused: bool,
    guard: InputGuard,
}
thread_local! { static INPUT_TARGET: std::cell::Cell<Option<InputTarget>> = const { std::cell::Cell::new(None) }; }
fn update_input(id: Identity, update: impl FnOnce(&mut InputTarget)) {
    INPUT_TARGET.with(|slot| {
        if let Some(mut target) = slot.get()
            && target.id == id
        {
            update(&mut target);
            slot.set(Some(target));
        }
    });
}
fn retire_input(id: Identity) {
    INPUT_TARGET.with(|slot| {
        if slot.get().is_some_and(|target| target.id == id) {
            slot.set(None);
        }
    });
}
fn contains(rect: RECT, point: POINT) -> bool {
    point.x >= rect.left && point.x < rect.right && point.y >= rect.top && point.y < rect.bottom
}
struct InputHooks {
    id: Identity,
    mouse: HHOOK,
    keys: HHOOK,
}
impl InputHooks {
    fn install(id: Identity, bounds: RECT, opener: RECT) -> Result<Self> {
        unsafe {
            let module = GetModuleHandleW(None)?;
            let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(observe_mouse), Some(module.into()), 0)?;
            let keys = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(observe_keyboard), Some(module.into()), 0) {
                Ok(keys) => keys,
                Err(error) => {
                    let _ = UnhookWindowsHookEx(mouse);
                    return Err(error);
                }
            };
            let mut buttons = 0;
            for (bit, key) in [
                (1, VK_LBUTTON),
                (2, VK_RBUTTON),
                (4, VK_MBUTTON),
                (8, VK_XBUTTON1),
                (16, VK_XBUTTON2),
            ] {
                if GetAsyncKeyState(key.0 as i32) < 0 {
                    buttons |= bit;
                }
            }
            let target = InputTarget {
                id,
                bounds,
                opener,
                focused: false,
                guard: InputGuard {
                    buttons,
                    escape: GetAsyncKeyState(VK_ESCAPE.0 as i32) < 0,
                },
            };
            // Controls are transient popups. Replacing the target retires the old
            // popup even if Windows had denied it foreground activation.
            if let Some(old) = INPUT_TARGET.with(|slot| slot.replace(Some(target))) {
                post_close(old.id, false);
            }
            Ok(Self { id, mouse, keys })
        }
    }
}
impl Drop for InputHooks {
    fn drop(&mut self) {
        retire_input(self.id);
        unsafe {
            let _ = UnhookWindowsHookEx(self.mouse);
            let _ = UnhookWindowsHookEx(self.keys);
        }
    }
}
// Like the notch observer, callbacks only latch input and post to the UI thread.
// They never consume input, borrow window state, query native state or draw.
unsafe extern "system" fn observe_mouse(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 {
        let event = unsafe { &*(lp.0 as *const MSLLHOOKSTRUCT) };
        let edge = match wp.0 as u32 {
            WM_LBUTTONDOWN => Some((1, true)),
            WM_LBUTTONUP => Some((1, false)),
            WM_RBUTTONDOWN => Some((2, true)),
            WM_RBUTTONUP => Some((2, false)),
            WM_MBUTTONDOWN => Some((4, true)),
            WM_MBUTTONUP => Some((4, false)),
            WM_XBUTTONDOWN | WM_XBUTTONUP => Some((
                if event.mouseData >> 16 == 1 { 8 } else { 16 },
                wp.0 as u32 == WM_XBUTTONDOWN,
            )),
            _ => None,
        };
        if let Some((bit, down)) = edge {
            INPUT_TARGET.with(|slot| {
                if let Some(mut target) = slot.get() {
                    let close = target.guard.button(
                        bit,
                        down,
                        contains(target.bounds, event.pt),
                        contains(target.opener, event.pt),
                    );
                    slot.set(Some(target));
                    if close {
                        let _ =
                            unsafe { PostMessageW(Some(target.id.hwnd), DISMISS, WPARAM(target.id.serial), LPARAM(0)) };
                    }
                }
            });
        }
    }
    unsafe { CallNextHookEx(None, code, wp, lp) }
}
unsafe extern "system" fn observe_keyboard(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 {
        let event = unsafe { &*(lp.0 as *const KBDLLHOOKSTRUCT) };
        if event.vkCode == VK_ESCAPE.0 as u32
            && matches!(wp.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP)
        {
            let down = matches!(wp.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            INPUT_TARGET.with(|slot| {
                if let Some(mut target) = slot.get() {
                    let pressed = target.guard.escape(down);
                    slot.set(Some(target));
                    if pressed {
                        let _ = unsafe {
                            PostMessageW(
                                Some(target.id.hwnd),
                                INPUT_ESCAPE,
                                WPARAM(target.id.serial),
                                LPARAM(isize::from(target.focused)),
                            )
                        };
                    }
                }
            });
        }
    }
    unsafe { CallNextHookEx(None, code, wp, lp) }
}
fn escape_returns_home(page: Page, focused: bool) -> bool {
    focused && !matches!(page, Page::Home | Page::Calendar)
}

/// The failed panel is never made visible or returned to its owner.
fn show_prepared<E>(
    prepared: std::result::Result<(), E>,
    show: impl FnOnce(),
    cleanup: impl FnOnce(),
) -> std::result::Result<(), E> {
    match prepared {
        Ok(()) => {
            show();
            Ok(())
        }
        Err(error) => {
            cleanup();
            Err(error)
        }
    }
}

pub struct OpenRequest {
    pub placement: Placement,
    pub state: State,
    pub hardware: control::Hardware,
    pub renderer: Renderer,
    pub material: (bool, crate::config::Backdrop, String, crate::theme::MaterialOptions),
    pub open_ms: u32,
    pub remote: bool,
}
impl OpenRequest {
    pub fn open(self) -> Result<HWND> {
        open_at(
            self.placement,
            self.state,
            self.hardware,
            self.renderer,
            (self.material.0, self.material.1, &self.material.2, self.material.3),
            self.open_ms,
            self.remote,
        )
    }
}

const WIDTH: f32 = 360.0;

#[derive(Clone)]
pub enum Action {
    Run(Command),
    Hardware(control::Request),
    Settings(&'static str),
    Project,
    QuickSettings,
    WispSettings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Home,
    Sound,
    Wifi,
    Bluetooth,
    Output,
    Mixer,
    Display,
    Edit,
    Calendar,
}

impl Page {
    fn hardware_request(self) -> Option<control::Request> {
        match self {
            Self::Wifi => Some(control::Request::Networks),
            Self::Display => Some(control::Request::Displays),
            Self::Bluetooth | Self::Sound | Self::Output => Some(control::Request::Batteries),
            _ => None,
        }
    }
}

fn network_empty_message(status: &crate::commands::wifi::ScanStatus) -> &'static str {
    use crate::commands::wifi::ScanStatus;
    match status {
        ScanStatus::Idle => "Checking nearby networks...",
        ScanStatus::Scanning => "Looking for networks...",
        ScanStatus::Ready => "No other networks found",
        ScanStatus::Failed(_) => "Use Refresh to try again",
    }
}
fn viewport_height(page: Page, content: f32, monitor_limit: f32) -> f32 {
    content
        .min(monitor_limit)
        .min(if page == Page::Wifi { 480. } else { f32::MAX })
}
fn page_request(page: Page, wifi_on: bool) -> Option<control::Request> {
    if page == Page::Wifi && !wifi_on {
        None
    } else {
        page.hardware_request()
    }
}

#[derive(Clone)]
enum Click {
    RefreshNetworks,
    Page(Page),
    Action(Action),
    Volume,
    Brightness(control::Display),
    AppVolume(u32),
    Seek(media::Timeline),
    Edit(usize),
    Calendar(super::calendar::Action),
}

#[derive(Clone, Copy)]
pub struct Bounds {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Bounds {
    fn hit(self, x: f32, y: f32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }
}

fn visible_scroll(scroll: f32, height: f32, max_scroll: f32, bounds: Bounds) -> f32 {
    let next = if bounds.y < scroll {
        bounds.y
    } else if bounds.y + bounds.h > scroll + height {
        bounds.y + bounds.h - height
    } else {
        scroll
    };
    next.clamp(0., max_scroll)
}
fn is_available_network(network: &crate::commands::wifi::Network, current: Option<&str>) -> bool {
    current != Some(network.name.as_str())
}

#[derive(Clone, Copy)]
pub enum Style {
    Card,
    Tile(bool),
    Row(bool),
    Slider(u32),
    Timeline(media::Timeline, bool),
    MediaTitle,
    MediaButton(bool),
    Text,
    Section,
    Header,
    Toggle(bool),
    CurrentRow,
    CompactCard(bool),
    Footer,
    CalendarDay {
        today: bool,
        selected: bool,
        in_month: bool,
    },
}

pub struct Element {
    pub bounds: Bounds,
    pub style: Style,
    pub title: String,
    pub detail: String,
    pub icon: &'static str,
    click: Option<Click>,
}

impl Element {
    pub fn is_interactive(&self) -> bool {
        self.click.is_some()
    }
}

struct Panel {
    launcher: HWND,
    dpi: u32,
    width: f32,
    height: f32,
    max_height: f32,
    bottom: i32,
    below: bool,
    state: State,
    hardware: control::Hardware,
    renderer: Renderer,
    palette: Palette,
    page: Page,
    elements: Vec<Element>,
    queue: VecDeque<Action>,
    hidden: u8,
    scroll: f32,
    focus: Option<usize>,
    dragging: Option<(usize, u32)>,
    remote: bool,
    remote_motion_ms: u32,
    page_fade_ms: u32,
    page_fade: Option<Instant>,
    surface: Option<SurfaceMotion>,
    surface_running: bool,
    calendar: super::calendar::Calendar,
    confirmed_wifi_on: bool,
    input: Option<InputHooks>,
    opener: RECT,
    paint_failed: bool,
    material: Option<crate::theme::SurfaceMaterial>,
    material_revision: u64,
    pending_rect: Option<RECT>,
    frame_presented: bool,
    foreground_prepared: bool,
}

struct SurfaceMotion {
    progress: crate::animation::Motion,
    duration_ms: u32,
    height: super::topbar::model::Spring,
    last: Instant,
    final_rect: RECT,
    anchor_x: i32,
    closing: bool,
}
impl SurfaceMotion {
    fn restart(&mut self, now: Instant) {
        self.last = now;
    }
    fn settle(&mut self) {
        self.progress.settle();
        self.height.settle();
    }
    fn scale(&self) -> f32 {
        super::topbar::model::surface_visual(self.progress.value).scale
    }
    fn rect(&self) -> RECT {
        let scale = self.scale();
        let base = self.final_rect;
        let left = base.left + ((self.anchor_x - base.left) as f32 * (1. - scale)).round() as i32;
        let top = base.top - (super::topbar::model::surface_visual(self.progress.value).rise).round() as i32;
        RECT {
            left,
            top,
            right: left + ((base.right - base.left) as f32 * scale).round() as i32,
            bottom: top + (self.height.value * scale).round() as i32,
        }
    }
}

fn preferences() -> std::path::PathBuf {
    crate::util::data_dir().join("control-center-sections")
}

impl Panel {
    fn focus_calendar_day(&mut self) {
        self.focus=self.elements.iter().position(|e|matches!(&e.click,Some(Click::Calendar(super::calendar::Action::Day(day))) if *day==self.calendar.focused));
        self.ensure_focus_visible();
    }
    fn focus_calendar_button(&mut self, action: &super::calendar::Action) {
        self.focus = self
            .elements
            .iter()
            .position(|e| matches!(&e.click,Some(Click::Calendar(candidate)) if candidate==action));
        self.ensure_focus_visible();
    }
    fn rebuild_calendar_focus(&mut self) {
        let button = self
            .focus
            .and_then(|i| self.elements.get(i))
            .and_then(|e| match &e.click {
                Some(Click::Calendar(action)) if !matches!(action, super::calendar::Action::Day(_)) => {
                    Some(action.clone())
                }
                _ => None,
            });
        self.rebuild();
        if let Some(action) = button {
            self.focus_calendar_button(&action);
        } else {
            self.focus_calendar_day();
        }
    }
    fn remember_calendar_focus(&mut self) {
        if let Some(day) = self
            .focus
            .and_then(|i| self.elements.get(i))
            .and_then(|e| match &e.click {
                Some(Click::Calendar(super::calendar::Action::Day(day))) => Some(*day),
                _ => None,
            })
        {
            self.calendar.focused = day;
        }
    }
    fn ensure_focus_visible(&mut self) {
        if let Some(bounds) = self.focus.and_then(|i| self.elements.get(i)).map(|e| e.bounds) {
            self.scroll = visible_scroll(self.scroll, self.height, self.max_scroll(), bounds);
        }
    }
    fn add(
        &mut self,
        bounds: Bounds,
        style: Style,
        title: impl Into<String>,
        detail: impl Into<String>,
        icon: &'static str,
        click: Option<Click>,
    ) {
        self.elements.push(Element {
            bounds,
            style,
            title: title.into(),
            detail: detail.into(),
            icon,
            click,
        });
    }

    fn card(&mut self, y: f32, h: f32, title: &str) {
        self.add(
            Bounds {
                x: 14.,
                y,
                w: self.width - 28.,
                h,
            },
            Style::Card,
            title,
            "",
            "",
            None,
        );
    }

    fn row(
        &mut self,
        y: f32,
        title: impl Into<String>,
        detail: impl Into<String>,
        icon: &'static str,
        active: bool,
        click: Option<Click>,
    ) {
        self.add(
            Bounds {
                x: 22.,
                y,
                w: self.width - 44.,
                h: 42.,
            },
            Style::Row(active),
            title,
            detail,
            icon,
            click,
        );
    }

    fn slider(&mut self, y: f32, label: &str, value: u32, icon: &'static str, click: Click) {
        self.add(
            Bounds {
                x: 29.,
                y,
                w: self.width - 58.,
                h: 32.,
            },
            Style::Slider(value),
            label,
            "",
            icon,
            Some(click),
        );
    }

    fn tile(
        &mut self,
        position: (u32, f32),
        title: impl Into<String>,
        detail: impl Into<String>,
        icon: &'static str,
        active: bool,
        click: Click,
    ) {
        let (column, y) = position;
        let w = (self.width - 40.) / 2.;
        self.add(
            Bounds {
                x: 14. + column as f32 * (w + 12.),
                y,
                w,
                h: 56.,
            },
            Style::Tile(active),
            title,
            detail,
            icon,
            Some(click),
        );
    }

    fn header(&mut self, title: &str) {
        self.row(10., title, "", "\u{E72B}", false, Some(Click::Page(Page::Home)));
    }

    fn rebuild(&mut self) {
        if self.dragging.is_some() {
            return;
        }
        self.elements.clear();
        match self.page {
            Page::Home => self.home(),
            Page::Calendar => self.calendar_page(),
            Page::Sound => self.sound_page(),
            Page::Wifi => self.wifi_page(),
            Page::Bluetooth => {
                self.header("Bluetooth");
                let mut y = 60.;
                if let Some(bt) = self.state.bluetooth.clone().flatten() {
                    self.row(
                        y,
                        if bt.on {
                            "Turn Bluetooth off"
                        } else {
                            "Turn Bluetooth on"
                        },
                        "",
                        glyph::BLUETOOTH,
                        bt.on,
                        Some(Click::Action(Action::Run(Command::BluetoothPower(!bt.on)))),
                    );
                    y += 46.;
                    for name in bt.connected {
                        let detail = self.battery_percent(&name).map_or_else(
                            || "Connected · battery not reported".into(),
                            |percent| format!("Connected · {percent}%"),
                        );
                        self.row(y, name, detail, glyph::BLUETOOTH, true, None);
                        y += 46.;
                    }
                }
                self.row(
                    y,
                    "Add or manage devices",
                    "",
                    "\u{E713}",
                    false,
                    Some(Click::Action(Action::Settings("ms-settings:bluetooth"))),
                );
            }
            Page::Output => {
                self.header("Sound output");
                for (i, output) in self.state.outputs.clone().into_iter().enumerate() {
                    self.row(
                        62. + i as f32 * 49.,
                        output.name,
                        if output.default { "✓" } else { "" },
                        glyph::SPEAKER,
                        output.default,
                        Some(Click::Action(Action::Run(Command::Output(output.id)))),
                    );
                }
                if self.state.outputs.is_empty() {
                    self.row(62., "No output devices", "", "", false, None);
                }
            }
            Page::Mixer => {
                self.header("Volume mixer");
                let mut y = 64.;
                for session in self.state.sessions.clone() {
                    self.add(
                        Bounds {
                            x: 29.,
                            y,
                            w: self.width - 58.,
                            h: 22.,
                        },
                        Style::Text,
                        session.name,
                        format!("{}%", session.volume),
                        "",
                        None,
                    );
                    self.slider(
                        y + 26.,
                        "App volume",
                        session.volume,
                        glyph::VOLUME,
                        Click::AppVolume(session.pid),
                    );
                    y += 70.;
                }
                if self.state.sessions.is_empty() {
                    self.row(y, "No apps are playing audio", "", "", false, None);
                }
            }
            Page::Display => {
                self.header("Display");
                self.row(
                    60.,
                    "Windows display settings",
                    "›",
                    "\u{E7F4}",
                    false,
                    Some(Click::Action(Action::Settings("ms-settings:display"))),
                );
                match self.hardware.displays.clone() {
                    None => self.row(110., "Checking brightness controls…", "", "", false, None),
                    Some(displays) if displays.is_empty() => {
                        self.row(110., "No monitor brightness controls available", "", "", false, None);
                    }
                    Some(displays) => {
                        for (i, display) in displays.into_iter().enumerate() {
                            let top = 113. + i as f32 * 65.;
                            self.add(
                                Bounds {
                                    x: 29.,
                                    y: top,
                                    w: self.width - 58.,
                                    h: 20.,
                                },
                                Style::Text,
                                &display.name,
                                format!("{}%", display.percent),
                                "",
                                None,
                            );
                            self.slider(
                                top + 22.,
                                "Brightness",
                                display.percent,
                                "\u{E706}",
                                Click::Brightness(display),
                            );
                        }
                    }
                }
            }
            Page::Edit => {
                self.header("Edit Control Center");
                for (row, (i, name)) in [(0, "Display"), (1, "Sound"), (3, "Now playing")]
                    .into_iter()
                    .enumerate()
                {
                    let shown = self.hidden & (1 << i) == 0;
                    self.row(
                        62. + row as f32 * 49.,
                        name,
                        if shown { "Shown" } else { "Hidden" },
                        "",
                        shown,
                        Some(Click::Edit(i)),
                    );
                }
            }
        }
        if self
            .focus
            .is_some_and(|i| !self.elements.get(i).is_some_and(|e| e.click.is_some()))
        {
            self.focus = None;
        }
        self.scroll = self.scroll.clamp(0., self.max_scroll());
    }

    fn panel_header(&mut self, title: &str, detail: &str, icon: &'static str) {
        self.add(
            Bounds {
                x: 22.,
                y: 14.,
                w: self.width - 112.,
                h: 44.,
            },
            Style::Header,
            title,
            detail,
            icon,
            None,
        );
    }
    fn footer(&mut self, y: f32, title: &str, click: Click) {
        self.add(
            Bounds {
                x: 22.,
                y,
                w: self.width - 44.,
                h: 36.,
            },
            Style::Footer,
            title,
            "",
            glyph::SETTINGS,
            Some(click),
        );
    }
    fn network_action(network: &crate::commands::wifi::Network) -> Click {
        network
            .profile
            .as_ref()
            .map(|id| Click::Action(Action::Run(Command::WifiConnect(id.clone()))))
            .unwrap_or(Click::Action(Action::Settings("ms-settings:network-wifi")))
    }
    fn wifi_page(&mut self) {
        let wifi = self.state.wifi.clone().flatten();
        self.panel_header(
            "Wi-Fi",
            wifi.as_ref().map_or(
                if self.state.wifi.is_none() {
                    "Checking adapter…"
                } else {
                    "Unavailable"
                },
                |wifi| {
                    if wifi.on {
                        "Wireless networks"
                    } else {
                        "Turn on to see networks"
                    }
                },
            ),
            glyph::WIFI,
        );
        if let Some(header) = self.elements.last_mut() {
            header.bounds.w = (self.width - 220.).max(100.);
        }
        if let Some(wifi) = &wifi {
            self.add(
                Bounds {
                    x: self.width - 82.,
                    y: 20.,
                    w: 60.,
                    h: 32.,
                },
                Style::Toggle(wifi.on),
                "Wi-Fi power",
                "",
                glyph::WIFI,
                Some(Click::Action(Action::Run(Command::WifiPower(!wifi.on)))),
            );
        }
        let scanning = matches!(self.hardware.network_scan, crate::commands::wifi::ScanStatus::Scanning);
        self.add(
            Bounds {
                x: self.width - 174.,
                y: 20.,
                w: 84.,
                h: 32.,
            },
            Style::Footer,
            if scanning { "Scanning..." } else { "Refresh" },
            "",
            "",
            if scanning || !wifi.as_ref().is_some_and(|w| w.on) {
                None
            } else {
                Some(Click::RefreshNetworks)
            },
        );
        let mut y = 78.;
        if let Some(wifi) = &wifi
            && wifi.on
        {
            if let Some(name) = &wifi.network {
                self.add(
                    Bounds {
                        x: 22.,
                        y,
                        w: self.width - 44.,
                        h: 52.,
                    },
                    Style::CurrentRow,
                    name,
                    "Connected",
                    glyph::WIFI,
                    None,
                );
                y += 64.;
            }
            self.add(
                Bounds {
                    x: 22.,
                    y,
                    w: self.width - 44.,
                    h: 24.,
                },
                Style::Text,
                "Available networks",
                "",
                "",
                None,
            );
            y += 32.;
            let scan_note = match &self.hardware.network_scan {
                crate::commands::wifi::ScanStatus::Failed(_) => Some("Could not refresh - Try again"),
                crate::commands::wifi::ScanStatus::Scanning => Some("Refreshing nearby networks..."),
                _ => None,
            };
            if let Some(note) = scan_note {
                self.add(
                    Bounds {
                        x: 22.,
                        y,
                        w: self.width - 44.,
                        h: 24.,
                    },
                    Style::Text,
                    note,
                    "",
                    "",
                    None,
                );
                y += 28.;
            }
            if self.hardware.networks.as_ref().is_some_and(|n| n.location_off) {
                self.add(
                    Bounds {
                        x: 22.,
                        y,
                        w: self.width - 44.,
                        h: 40.,
                    },
                    Style::Text,
                    "Location is off - Showing saved networks",
                    "",
                    "",
                    None,
                );
                y += 44.;
            }

            match self.hardware.networks.clone() {
                None => {
                    self.add(
                        Bounds {
                            x: 22.,
                            y,
                            w: self.width - 44.,
                            h: 44.,
                        },
                        Style::Text,
                        network_empty_message(&self.hardware.network_scan),
                        "",
                        "",
                        None,
                    );
                    y += 56.;
                }
                Some(networks) => {
                    let list = networks
                        .list
                        .iter()
                        .filter(|network| is_available_network(network, wifi.network.as_deref()))
                        .take(15)
                        .collect::<Vec<_>>();
                    let columns = if self.width < 360. { 1 } else { 2 };
                    let w = (self.width - 44. - (columns - 1) as f32 * 12.) / columns as f32;
                    for (index, network) in list.iter().enumerate() {
                        let detail = if network.profile.is_some() {
                            "Saved"
                        } else if network.secured {
                            "Secured"
                        } else {
                            "Open network"
                        };
                        self.add(
                            Bounds {
                                x: 22. + (index % columns) as f32 * (w + 12.),
                                y: y + (index / columns) as f32 * 64.,
                                w,
                                h: 56.,
                            },
                            Style::CompactCard(false),
                            &network.name,
                            detail,
                            glyph::WIFI,
                            Some(Self::network_action(network)),
                        );
                    }
                    if list.is_empty() {
                        self.add(
                            Bounds {
                                x: 22.,
                                y,
                                w: self.width - 44.,
                                h: 40.,
                            },
                            Style::Text,
                            network_empty_message(&self.hardware.network_scan),
                            "",
                            "",
                            None,
                        );
                        y += 48.;
                    } else {
                        y += list.len().div_ceil(columns) as f32 * 64.;
                    }
                }
            }
        } else {
            self.add(
                Bounds {
                    x: 22.,
                    y,
                    w: self.width - 44.,
                    h: 52.,
                },
                Style::CurrentRow,
                if wifi.is_some() {
                    "Wi-Fi is off"
                } else if self.state.wifi.is_none() {
                    "Checking Wi-Fi…"
                } else {
                    "No Wi-Fi adapter"
                },
                "",
                glyph::WIFI,
                None,
            );
            y += 64.;
        }
        self.footer(
            y + 4.,
            "Windows Wi-Fi settings",
            Click::Action(Action::Settings("ms-settings:network-wifi")),
        );
    }
    fn sound_page(&mut self) {
        self.panel_header("Sound", "Output and volume", glyph::SPEAKER);
        if let Some(audio) = self.state.audio.clone().flatten() {
            self.add(
                Bounds {
                    x: self.width - 122.,
                    y: 18.,
                    w: 100.,
                    h: 36.,
                },
                Style::Footer,
                if audio.muted { "Unmute" } else { "Mute" },
                "",
                if audio.muted { glyph::MUTED } else { glyph::VOLUME },
                Some(Click::Action(Action::Run(Command::Mute(!audio.muted)))),
            );
            self.add(
                Bounds {
                    x: 22.,
                    y: 78.,
                    w: self.width - 44.,
                    h: 52.,
                },
                Style::CurrentRow,
                &audio.device,
                self.output_battery(&audio.device).map_or_else(
                    || format!("Volume {}%", audio.volume),
                    |percent| format!("Volume {}% · Battery {percent}%", audio.volume),
                ),
                if audio.muted { glyph::MUTED } else { glyph::VOLUME },
                None,
            );
            self.slider(
                136.,
                "Volume",
                audio.volume,
                if audio.muted { glyph::MUTED } else { glyph::VOLUME },
                Click::Volume,
            );
            self.add(
                Bounds {
                    x: 22.,
                    y: 176.,
                    w: self.width - 44.,
                    h: 24.,
                },
                Style::Text,
                "Output devices",
                "",
                "",
                None,
            );
            let columns = if self.width < 360. { 1 } else { 2 };
            let w = (self.width - 44. - (columns - 1) as f32 * 12.) / columns as f32;
            for (index, output) in self.state.outputs.clone().into_iter().enumerate() {
                self.add(
                    Bounds {
                        x: 22. + (index % columns) as f32 * (w + 12.),
                        y: 206. + (index / columns) as f32 * 64.,
                        w,
                        h: 56.,
                    },
                    Style::CompactCard(output.default),
                    &output.name,
                    self.output_battery(&output.name).map_or_else(
                        || {
                            if output.default {
                                "Selected".into()
                            } else {
                                "Switch output".into()
                            }
                        },
                        |percent| {
                            format!(
                                "{} · {percent}% battery",
                                if output.default { "Selected" } else { "Switch output" }
                            )
                        },
                    ),
                    glyph::SPEAKER,
                    Some(Click::Action(Action::Run(Command::Output(output.id)))),
                );
            }
            let y = 206. + self.state.outputs.len().max(1).div_ceil(columns) as f32 * 64.;
            if self.state.outputs.is_empty() {
                self.add(
                    Bounds {
                        x: 22.,
                        y: 206.,
                        w: self.width - 44.,
                        h: 44.,
                    },
                    Style::Text,
                    "No output devices available",
                    "",
                    "",
                    None,
                );
            }
            self.footer(y, "Volume mixer", Click::Page(Page::Mixer));
        } else {
            self.add(
                Bounds {
                    x: 22.,
                    y: 78.,
                    w: self.width - 44.,
                    h: 52.,
                },
                Style::CurrentRow,
                if self.state.audio.is_none() {
                    "Checking sound…"
                } else {
                    "No sound output"
                },
                "Connect an output device",
                glyph::SPEAKER,
                None,
            );
            self.footer(
                142.,
                "Windows sound settings",
                Click::Action(Action::Settings("ms-settings:sound")),
            );
        }
    }
    fn calendar_page(&mut self) {
        let month = self.calendar.month;
        let away = month != self.calendar.today.month();
        let today_button = if away { 116. } else { 72. };
        self.add(
            Bounds {
                x: 22.,
                y: 14.,
                w: self.width - 128.,
                h: 42.,
            },
            Style::Header,
            super::calendar::month_label(month),
            "",
            "",
            None,
        );
        for (x, delta, icon) in [(self.width - 96., -1, "\u{E76B}"), (self.width - 56., 1, "\u{E76C}")] {
            self.add(
                Bounds {
                    x,
                    y: 18.,
                    w: 34.,
                    h: 34.,
                },
                Style::Footer,
                "",
                "",
                icon,
                Some(Click::Calendar(super::calendar::Action::Month(delta))),
            );
        }
        self.add(
            Bounds {
                x: 22.,
                y: 64.,
                w: today_button,
                h: 30.,
            },
            Style::Footer,
            if away { "Back to today" } else { "Today" },
            "",
            "",
            Some(Click::Calendar(super::calendar::Action::Today)),
        );
        self.add(
            Bounds {
                x: 32. + today_button,
                y: 64.,
                w: self.width - 54. - today_button,
                h: 30.,
            },
            Style::Text,
            super::calendar::long_label(self.calendar.today),
            "",
            "",
            None,
        );
        let cell = (self.width - 36.) / 7.;
        for column in 0..7 {
            self.add(
                Bounds {
                    x: 18. + column as f32 * cell,
                    y: 108.,
                    w: cell,
                    h: 28.,
                },
                Style::CalendarDay {
                    today: false,
                    selected: false,
                    in_month: false,
                },
                super::calendar::weekday_label((self.calendar.first_weekday + column as u8) % 7),
                "",
                "",
                None,
            );
        }
        for (index, date) in month.grid(self.calendar.first_weekday).into_iter().enumerate() {
            if let Some(date) = date {
                self.add(
                    Bounds {
                        x: 18. + (index % 7) as f32 * cell,
                        y: 140. + (index / 7) as f32 * 44.,
                        w: cell,
                        h: 40.,
                    },
                    Style::CalendarDay {
                        today: date == self.calendar.today,
                        selected: date == self.calendar.selected,
                        in_month: date.month() == month,
                    },
                    super::calendar::day_label(date),
                    "",
                    "",
                    Some(Click::Calendar(super::calendar::Action::Day(date))),
                );
            }
        }
        self.add(
            Bounds {
                x: 22.,
                y: 414.,
                w: self.width - 44.,
                h: 34.,
            },
            Style::Text,
            format!("Selected: {}", super::calendar::long_label(self.calendar.selected)),
            "",
            "",
            None,
        );
    }
    fn battery_percent(&self, name: &str) -> Option<u8> {
        let mut matches = self
            .hardware
            .batteries
            .iter()
            .filter(|(device, _)| device.eq_ignore_ascii_case(name));
        let (_, percent) = matches.next()?;
        if matches.next().is_some() { None } else { *percent }
    }

    fn output_battery(&self, output: &str) -> Option<u8> {
        let output = output.to_lowercase();
        let mut connected = self
            .state
            .bluetooth
            .as_ref()?
            .as_ref()?
            .connected
            .iter()
            .filter(|name| {
                let name = name.to_lowercase();
                !name.is_empty() && (output == name || output.ends_with(&format!("({name})")))
            });
        let device = connected.next()?;
        if connected.next().is_some() {
            None
        } else {
            self.battery_percent(device)
        }
    }

    fn home(&mut self) {
        self.add(
            Bounds {
                x: 20.,
                y: 12.,
                w: self.width - 40.,
                h: 28.,
            },
            Style::Header,
            "Controls",
            "",
            "",
            None,
        );
        let wifi = self.state.wifi.clone().flatten();
        let title = wifi
            .as_ref()
            .and_then(|w| w.network.clone())
            .unwrap_or_else(|| "Wi-Fi".into());
        self.tile(
            (0, 48.),
            title,
            wifi.as_ref().map_or(
                if self.state.wifi.is_none() {
                    "Checking..."
                } else {
                    "Unavailable"
                },
                |w| {
                    if w.network.is_some() {
                        "Connected"
                    } else if w.on {
                        "On"
                    } else {
                        "Off"
                    }
                },
            ),
            glyph::WIFI,
            wifi.is_some_and(|w| w.on),
            Click::Page(Page::Wifi),
        );
        let bt = self.state.bluetooth.clone().flatten();
        let detail = bt.as_ref().map_or_else(
            || {
                if self.state.bluetooth.is_none() {
                    "Checking...".into()
                } else {
                    "Unavailable".into()
                }
            },
            |bt| {
                if let Some(name) = bt.connected.first() {
                    self.battery_percent(name)
                        .map_or_else(|| name.clone(), |percent| format!("{name} · {percent}%"))
                } else if bt.on {
                    "On".into()
                } else {
                    "Off".into()
                }
            },
        );
        self.tile(
            (1, 48.),
            "Bluetooth",
            detail,
            glyph::BLUETOOTH,
            bt.is_some_and(|bt| bt.on),
            Click::Page(Page::Bluetooth),
        );
        self.tile(
            (0, 112.),
            "Do not disturb",
            "Windows settings",
            "\u{E708}",
            false,
            Click::Action(Action::Settings("ms-settings:notifications")),
        );
        self.tile(
            (1, 112.),
            "Project",
            "Display mode",
            "\u{E7F4}",
            false,
            Click::Action(Action::Project),
        );
        let mut y = 180.;
        if self.hidden & 2 == 0 {
            self.add(
                Bounds {
                    x: 22.,
                    y,
                    w: self.width - 100.,
                    h: 28.,
                },
                Style::Section,
                "Sound",
                "",
                "",
                Some(Click::Page(Page::Sound)),
            );
            if let Some(audio) = self.state.audio.clone().flatten() {
                self.add(
                    Bounds {
                        x: self.width - 70.,
                        y,
                        w: 47.,
                        h: 25.,
                    },
                    Style::Text,
                    format!("{}%", audio.volume),
                    "",
                    "",
                    None,
                );
                self.slider(
                    y + 30.,
                    "Volume",
                    audio.volume,
                    if audio.muted { glyph::MUTED } else { glyph::VOLUME },
                    Click::Volume,
                );
                let output_detail = self
                    .output_battery(&audio.device)
                    .map_or_else(|| "›".into(), |percent| format!("{percent}% ›"));
                self.add(
                    Bounds {
                        x: 29.,
                        y: y + 70.,
                        w: self.width - 150.,
                        h: 32.,
                    },
                    Style::Row(false),
                    audio.device,
                    output_detail,
                    glyph::SPEAKER,
                    Some(Click::Page(Page::Output)),
                );
                self.add(
                    Bounds {
                        x: self.width - 110.,
                        y: y + 70.,
                        w: 81.,
                        h: 32.,
                    },
                    Style::Row(false),
                    "Mixer",
                    "",
                    "",
                    Some(Click::Page(Page::Mixer)),
                );
            } else {
                self.row(
                    y + 30.,
                    "No sound output available",
                    "",
                    "",
                    false,
                    Some(Click::Page(Page::Sound)),
                );
            }
            y += 118.;
        }
        if self.hidden & 8 == 0
            && let Some(media) = self.state.media.clone().flatten()
        {
            let timeline = media.timeline;
            let height = if timeline.is_some() { 143. } else { 97. };
            self.card(y, height, "");
            let width = self.width - 58.;
            self.add(
                Bounds {
                    x: 29.,
                    y: y + 11.,
                    w: width - 134.,
                    h: 25.,
                },
                Style::MediaTitle,
                if media.title.is_empty() {
                    media.app.clone()
                } else {
                    media.title
                },
                "",
                "",
                None,
            );
            self.add(
                Bounds {
                    x: 29.,
                    y: y + 35.,
                    w: width - 134.,
                    h: 23.,
                },
                Style::MediaTitle,
                if media.artist.is_empty() {
                    media.app
                } else {
                    media.artist
                },
                "subtitle",
                "",
                None,
            );
            for (i, (icon, action)) in [
                (glyph::PREVIOUS, media::Control::Previous),
                (
                    if media.playing { glyph::PAUSE } else { glyph::PLAY },
                    media::Control::PlayPause,
                ),
                (glyph::NEXT, media::Control::Next),
            ]
            .into_iter()
            .enumerate()
            {
                self.add(
                    Bounds {
                        x: self.width - 155. + i as f32 * 43.,
                        y: y + 17.,
                        w: 40.,
                        h: 40.,
                    },
                    Style::MediaButton(i == 1),
                    "",
                    "",
                    icon,
                    Some(Click::Action(Action::Run(Command::Media(action)))),
                );
            }
            if let Some(timeline) = timeline {
                self.add(
                    Bounds {
                        x: 29.,
                        y: y + 74.,
                        w: width,
                        h: 52.,
                    },
                    Style::Timeline(timeline, media.playing),
                    "",
                    "",
                    "",
                    timeline.seek.map(|_| Click::Seek(timeline)),
                );
            }
            y += height + 12.;
        }
        let display = self.hidden & 1 == 0;
        let settings_x = self.width - if display { 170. } else { 130. };
        self.add(
            Bounds {
                x: 14.,
                y,
                w: settings_x - 18.,
                h: 32.,
            },
            Style::Text,
            "More Windows controls",
            "",
            "",
            Some(Click::Action(Action::QuickSettings)),
        );
        self.add(
            Bounds {
                x: settings_x,
                y,
                w: 32.,
                h: 32.,
            },
            Style::Row(false),
            "",
            "",
            "\u{E713}",
            Some(Click::Action(Action::WispSettings)),
        );
        if display {
            self.add(
                Bounds {
                    x: self.width - 130.,
                    y,
                    w: 32.,
                    h: 32.,
                },
                Style::Row(false),
                "",
                "",
                "\u{E7F4}",
                Some(Click::Page(Page::Display)),
            );
        }
        self.add(
            Bounds {
                x: self.width - 90.,
                y,
                w: 76.,
                h: 32.,
            },
            Style::Row(false),
            "Edit",
            "",
            "\u{E70F}",
            Some(Click::Page(Page::Edit)),
        );
    }

    fn content_height(&self) -> f32 {
        self.elements.iter().map(|e| e.bounds.y + e.bounds.h).fold(0., f32::max) + 14.
    }

    fn fit(&mut self, hwnd: HWND) {
        let height = viewport_height(self.page, self.content_height(), self.max_height);
        if (height - self.height).abs() < 1. {
            return;
        }
        self.height = height;
        self.scroll = self.scroll.min(self.max_scroll());
        let scale = self.dpi as f32 / 96.;
        let width = (self.width * scale).round() as i32;
        let height = (height * scale).round() as i32;
        if let Some(surface) = &mut self.surface {
            surface.height.target = height as f32;
            surface.final_rect.bottom = surface.final_rect.top + height;
            self.start_surface_timer(hwnd);
            return;
        }
        let mut rect = RECT::default();
        if unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok() {
            let top = if self.below { self.bottom } else { self.bottom - height };
            self.pending_rect = Some(RECT {
                left: rect.left,
                top,
                right: rect.left + width,
                bottom: top + height,
            });
        }
    }

    fn sync_input_bounds(&self, hwnd: HWND) {
        if let Some(id) = identity(hwnd) {
            let mut bounds = RECT::default();
            if unsafe { GetWindowRect(hwnd, &mut bounds) }.is_ok() {
                update_input(id, |target| target.bounds = bounds);
            }
        }
    }
    fn present(&mut self, hwnd: HWND) -> Result<()> {
        self.foreground_prepared = false;
        self.sync_input_bounds(hwnd);
        let max_scroll = self.max_scroll();
        let remote_alpha = self.remote.then(|| self.remote_alpha());
        let page_opacity = self.page_fade.filter(|_| !self.remote).map_or(1., |start| {
            let t = (start.elapsed().as_secs_f32() * 1000. / self.page_fade_ms.max(1) as f32).min(1.);
            0.55 + 0.45 * crate::animation::ease(t)
        });
        self.renderer.draw_control(
            hwnd,
            self.dpi,
            &self.palette,
            &self.elements,
            self.scroll,
            max_scroll,
            self.focus,
            self.dragging,
            (
                page_opacity,
                self.surface.as_ref().map_or(1., SurfaceMotion::scale),
                self.surface.as_ref().map_or(1., |s| s.progress.value.clamp(0., 1.)),
            ),
            remote_alpha,
        )?;
        self.frame_presented = true;
        self.foreground_prepared = true;
        if self.remote {
            // A direct dirty-frame refresh replaces the pending paint as well.
            unsafe {
                let _ = ValidateRect(Some(hwnd), None);
            }
        }
        Ok(())
    }
    fn remote_alpha(&self) -> u8 {
        self.page_fade.map_or(255, |start| {
            super::RemotePopupFade::alpha_at(start.elapsed(), self.page_fade_ms)
        })
    }
    fn present_remote_tick(&mut self, hwnd: HWND) -> Result<()> {
        self.foreground_prepared = false;
        if !unsafe { GetUpdateRect(hwnd, None, false) }.as_bool()
            && self.renderer.cached_popup_matches(hwnd, self.dpi)?
        {
            self.renderer
                .present_cached_popup(hwnd, self.dpi, self.remote_alpha())?;
            self.frame_presented = true;
            self.foreground_prepared = true;
            Ok(())
        } else {
            self.present(hwnd)
        }
    }

    fn max_scroll(&self) -> f32 {
        (self.content_height() - self.height).max(0.)
    }

    fn activate(&mut self, click: Click, hwnd: HWND, value: Option<u32>) {
        let action = match click {
            Click::RefreshNetworks => Some(Action::Hardware(control::Request::Networks)),
            Click::Page(page) => {
                self.page = page;
                let ms = if self.remote {
                    self.remote_motion_ms.min(105)
                } else {
                    105
                };
                let interval = if self.remote {
                    super::RemotePopupFade::INTERVAL
                } else {
                    15
                };
                if ms != 0
                    && super::client_animations_enabled()
                    && unsafe { SetTimer(Some(hwnd), PAGE_FADE, interval, None) } != 0
                {
                    self.page_fade_ms = ms;
                    self.page_fade = Some(Instant::now());
                }
                self.scroll = 0.;
                self.focus = None;
                self.rebuild();
                page_request(page, self.confirmed_wifi_on).map(Action::Hardware)
            }
            Click::Action(action) => Some(action),
            Click::Volume => value.map(|v| Action::Run(Command::Volume(v))).or_else(|| {
                self.state
                    .audio
                    .as_ref()
                    .and_then(Option::as_ref)
                    .map(|a| Action::Run(Command::Mute(!a.muted)))
            }),
            Click::Brightness(display) => value.map(|v| Action::Hardware(control::Request::Brightness(display, v))),
            Click::AppVolume(pid) => value.map(|percent| Action::Run(Command::AppVolume { pid, percent })),
            Click::Seek(timeline) => value.zip(timeline.seek).and_then(|(percent, (min, max))| {
                let target =
                    timeline.start + ((timeline.end - timeline.start) as f64 * f64::from(percent) / 100.) as i64;
                let track = self.state.media.as_ref()?.as_ref()?;
                Some(Action::Run(Command::Media(media::Control::Seek {
                    source: track.source.clone(),
                    title: track.title.clone(),
                    position: target.clamp(min, max),
                })))
            }),
            Click::Edit(index) => {
                self.hidden ^= 1 << index;
                if let Err(error) = std::fs::write(preferences(), self.hidden.to_string()) {
                    eprintln!("Control Center sections: {error}");
                }
                self.rebuild();
                None
            }
            Click::Calendar(action) => {
                if matches!(action, super::calendar::Action::Today) {
                    self.calendar.refresh();
                }
                let browse = matches!(action, super::calendar::Action::Month(_));
                self.calendar.apply(action.clone());
                self.rebuild();
                if browse {
                    self.focus_calendar_button(&action);
                } else {
                    self.focus_calendar_day();
                }
                None
            }
        };
        if let Some(action) = action {
            self.queue.push_back(action);
            unsafe {
                let _ = PostMessageW(Some(self.launcher), WM_SOUND_ACTION, WPARAM(hwnd.0 as usize), LPARAM(0));
            }
        }
    }

    fn pointer(&self, lp: LPARAM) -> (f32, f32) {
        let scale = 96. / self.dpi as f32 / self.surface.as_ref().map_or(1., SurfaceMotion::scale);
        (
            (lp.0 as i16) as f32 * scale,
            ((lp.0 >> 16) as i16) as f32 * scale + self.scroll,
        )
    }

    fn slider_value(&self, index: usize, x: f32) -> u32 {
        let b = self.elements[index].bounds;
        if matches!(self.elements[index].style, Style::Timeline(..)) {
            return (((x - b.x) / b.w).clamp(0., 1.) * 100.).round() as u32;
        }
        (((x - b.x - 36.) / (b.w - 44.)).clamp(0., 1.) * 100.).round() as u32
    }

    fn start_surface_timer(&mut self, hwnd: HWND) {
        if !super::client_animations_enabled() {
            if let Some(surface) = &mut self.surface {
                surface.settle();
            }
            self.place_surface(hwnd);
            return;
        }
        if self.surface_running {
            return;
        }
        if let Some(surface) = &mut self.surface {
            surface.restart(Instant::now());
        }
        if unsafe {
            SetTimer(
                Some(hwnd),
                SURFACE_MOTION,
                crate::animation::interval(self.remote),
                None,
            )
        } == 0
        {
            if let Some(surface) = &mut self.surface {
                surface.settle();
            }
            self.surface_running = false;
            self.place_surface(hwnd);
            if self.surface.as_ref().is_some_and(|s| s.closing) {
                request_close(hwnd, true);
            }
        } else {
            self.surface_running = true;
        }
    }
    fn place_surface(&mut self, hwnd: HWND) {
        let Some(surface) = &self.surface else { return };
        self.pending_rect = Some(surface.rect());
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
    fn begin_close(&mut self, hwnd: HWND) -> bool {
        let Some(surface) = &mut self.surface else { return true };
        if !super::client_animations_enabled() {
            return true;
        }
        if surface.closing {
            return false;
        }
        surface.closing = true;
        surface.progress.retarget_duration(false, surface.duration_ms);
        surface.last = Instant::now();
        self.dragging = None;
        let _ = unsafe { ReleaseCapture() };
        self.start_surface_timer(hwnd);
        false
    }
}

/// The HWND owns one strong reference. Every callback/public entry clones it
/// before borrowing so nested native destruction cannot invalidate live state.
struct NativeState<T>(std::marker::PhantomData<T>);
impl<T> NativeState<T> {
    fn into_raw(value: T) -> *const RefCell<T> {
        Rc::into_raw(Rc::new(RefCell::new(value)))
    }
    fn install<E>(
        value: T,
        install: impl FnOnce(*const RefCell<T>) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        let pointer = Self::into_raw(value);
        if let Err(error) = install(pointer) {
            unsafe {
                Self::release(pointer);
            }
            return Err(error);
        }
        Ok(())
    }
    unsafe fn acquire(pointer: *const RefCell<T>) -> Option<Rc<RefCell<T>>> {
        if pointer.is_null() {
            return None;
        }
        unsafe {
            Rc::increment_strong_count(pointer);
            Some(Rc::from_raw(pointer))
        }
    }
    unsafe fn release(pointer: *const RefCell<T>) {
        if !pointer.is_null() {
            drop(unsafe { Rc::from_raw(pointer) });
        }
    }
}
fn panel(hwnd: HWND) -> Option<Rc<RefCell<Panel>>> {
    identity(hwnd)?;
    // All callers are on the owning window thread. Neither lookup dispatches
    // messages, so the native ownership reference stays live until acquired.
    unsafe { NativeState::acquire(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<Panel>) }
}

fn after_borrow<T, R>(borrow: std::cell::RefMut<'_, T>, dispatch: impl FnOnce() -> R) -> R {
    drop(borrow);
    dispatch()
}
fn state_is_current(hwnd: HWND, owner: &Rc<RefCell<Panel>>) -> bool {
    identity(hwnd).is_some()
        && unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<Panel> } == Rc::as_ptr(owner)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    Home,
    Wifi,
    Sound,
    Calendar,
}

impl Entry {
    fn page(self) -> Page {
        match self {
            Self::Home => Page::Home,
            Self::Wifi => Page::Wifi,
            Self::Sound => Page::Sound,
            Self::Calendar => Page::Calendar,
        }
    }
}

pub struct Placement {
    pub owner: HWND,
    pub launcher: HWND,
    pub anchor: RECT,
    pub dpi: u32,
    pub entry: Entry,
    pub below: bool,
}

pub fn open_at(
    placement: Placement,
    state: State,
    hardware: control::Hardware,
    renderer: Renderer,
    material: (bool, crate::config::Backdrop, &str, crate::theme::MaterialOptions),
    open_ms: u32,
    remote: bool,
) -> Result<HWND> {
    let opened_at = Instant::now();
    let Placement {
        owner,
        launcher,
        anchor,
        dpi,
        entry,
        below,
    } = placement;
    unsafe {
        let instance = GetModuleHandleW(None)?.into();
        let class = WNDCLASSW {
            hInstance: instance,
            lpfnWndProc: Some(wndproc),
            lpszClassName: w!("Wisp.ControlCenter"),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            return Err(Error::from_thread());
        }
        let monitor = MonitorFromWindow(owner, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return Err(Error::from_thread());
        }
        let scale = dpi as f32 / 96.;
        let width = (((if entry == Entry::Calendar {
            392.
        } else if entry == Entry::Home {
            WIDTH
        } else {
            424.
        }) * scale)
            .round() as i32)
            .min(info.rcWork.right - info.rcWork.left - 16);
        let bottom = if below {
            (anchor.bottom + 8).max(info.rcWork.top + 8)
        } else {
            (anchor.top - 8).min(info.rcWork.bottom - 8)
        };
        let height = if below {
            info.rcWork.bottom - bottom - 8
        } else {
            bottom - info.rcWork.top - 8
        };
        if width <= 0 || height <= 0 {
            return Err(Error::from_hresult(windows::Win32::Foundation::E_FAIL));
        }
        let x = (anchor.left + (anchor.right - anchor.left - width) / 2).clamp(
            info.rcWork.left + 8,
            (info.rcWork.right - width - 8).max(info.rcWork.left + 8),
        );

        let mut contents = Panel {
            launcher,
            dpi,
            width: width as f32 / scale,
            height: height as f32 / scale,
            max_height: height as f32 / scale,
            bottom,
            below,
            confirmed_wifi_on: state.wifi.as_ref().and_then(|w| w.as_ref()).is_some_and(|w| w.on),
            state,
            hardware,
            renderer,
            palette: Palette::new(true, false),
            page: entry.page(),
            elements: Vec::new(),
            queue: VecDeque::new(),
            hidden: std::fs::read_to_string(preferences())
                .ok()
                .and_then(|s| s.trim().parse::<u8>().ok())
                .unwrap_or(0)
                & 15,
            scroll: 0.,
            focus: None,
            dragging: None,
            remote,
            remote_motion_ms: open_ms.min(120),
            page_fade_ms: if remote { open_ms.min(120) } else { open_ms },
            page_fade: None,
            surface: None,
            surface_running: false,
            calendar: super::calendar::Calendar::new(),
            input: None,
            opener: anchor,
            paint_failed: false,
            material: None,
            material_revision: 0,
            pending_rect: None,
            frame_presented: false,
            foreground_prepared: false,
        };
        contents.rebuild();
        contents.height = viewport_height(contents.page, contents.content_height(), contents.max_height);
        if entry == Entry::Calendar {
            contents.focus_calendar_day();
        }
        let height = (contents.height * scale).round() as i32;
        let y = if below { bottom } else { bottom - height };
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_LAYERED,
            class.lpszClassName,
            if entry == Entry::Calendar {
                w!("Calendar")
            } else {
                w!("Control Center")
            },
            WS_POPUP,
            x,
            y,
            width,
            height,
            Some(owner),
            None,
            Some(instance),
            None,
        )?;
        let serial = NEXT_IDENTITY.fetch_add(1, std::sync::atomic::Ordering::Relaxed).max(1);
        if let Err(error) = SetPropW(hwnd, IDENTITY, Some(HANDLE(serial as *mut _))) {
            let _ = DestroyWindow(hwnd);
            return Err(error);
        }
        let created = Identity { hwnd, serial };
        let installed = NativeState::install(contents, |pointer| {
            SetLastError(ERROR_SUCCESS);
            let previous = SetWindowLongPtrW(hwnd, GWLP_USERDATA, pointer as isize);
            if previous == 0 && GetLastError() != ERROR_SUCCESS {
                Err(Error::from_thread())
            } else {
                Ok(())
            }
        });
        if let Err(error) = installed {
            if is_current(created) {
                let _ = RemovePropW(hwnd, IDENTITY);
                let _ = DestroyWindow(hwnd);
            }
            return Err(error);
        }
        apply_theme(hwnd, material.0, material.1, material.2, material.3);
        if !is_current(created) {
            return Err(Error::from_hresult(windows::Win32::Foundation::E_ABORT));
        }

        if below && open_ms != 0 && super::client_animations_enabled() {
            let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style | WS_EX_LAYERED.0 as isize);
            if let Some(cell) = panel(hwnd) {
                let mut p = cell.borrow_mut();
                // The prepared first frame must contain visible pixels even
                // before the first animation timer reaches this window.
                let mut progress = crate::animation::Motion::default();
                progress.value = 0.08;
                progress.retarget_duration(true, open_ms);
                p.surface = Some(SurfaceMotion {
                    progress,
                    duration_ms: open_ms,
                    height: super::topbar::model::Spring::new(height as f32),
                    last: Instant::now(),
                    final_rect: RECT {
                        left: x,
                        top: y,
                        right: x + width,
                        bottom: y + height,
                    },
                    anchor_x: (anchor.left + anchor.right) / 2,
                    closing: false,
                });
                p.place_surface(hwnd);
                p.start_surface_timer(hwnd);
            }
        }
        if open_ms != 0
            && !below
            && super::client_animations_enabled()
            && SetTimer(
                Some(hwnd),
                PAGE_FADE,
                if remote { super::RemotePopupFade::INTERVAL } else { 15 },
                None,
            ) != 0
            && let Some(cell) = panel(hwnd)
        {
            cell.borrow_mut().page_fade = Some(Instant::now());
        }
        if !is_current(created) {
            return Err(Error::from_hresult(windows::Win32::Foundation::E_ABORT));
        }
        flush_layout(hwnd);
        let prepared = (|| {
            let result = panel(hwnd)
                .ok_or_else(|| Error::from_hresult(E_ABORT))?
                .borrow_mut()
                .present(hwnd);
            if let Err(error) = &result {
                crate::logging::event("controls.present.failed", format_args!("serial={serial} error={error}"));
            }
            result?;
            if !publish_material_frame(hwnd) {
                // Resolve a failed host to a readable frame before first show.
                panel(hwnd)
                    .ok_or_else(|| Error::from_hresult(E_ABORT))?
                    .borrow_mut()
                    .present(hwnd)?;
                publish_material_frame(hwnd);
            }
            if !is_current(created) {
                return Err(Error::from_hresult(E_ABORT));
            }
            let mut bounds = RECT::default();
            GetWindowRect(hwnd, &mut bounds)?;
            let hooks = InputHooks::install(created, bounds, anchor).inspect_err(|error| {
                crate::logging::event("controls.input.failed", format_args!("serial={serial} error={error}"));
            })?;
            panel(hwnd)
                .ok_or_else(|| Error::from_hresult(E_ABORT))?
                .borrow_mut()
                .input = Some(hooks);
            Ok(())
        })();
        show_prepared(
            prepared,
            || {
                crate::logging::event(
                    "controls.prepared",
                    format_args!("serial={serial} open_us={}", opened_at.elapsed().as_micros()),
                );
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            },
            || destroy(created),
        )?;
        if !is_current(created) {
            return Err(Error::from_hresult(E_ABORT));
        }
        if !IsWindowVisible(hwnd).as_bool() {
            crate::logging::event("controls.show.failed", format_args!("serial={serial}"));
            destroy(created);
            return Err(Error::from_hresult(E_FAIL));
        }
        let activated = SetForegroundWindow(hwnd).as_bool();
        update_input(created, |target| target.focused = GetForegroundWindow() == hwnd);
        crate::logging::event(
            "controls.shown",
            format_args!(
                "serial={serial} activated={activated} open_us={}",
                opened_at.elapsed().as_micros()
            ),
        );
        if !is_current(created) {
            return Err(Error::from_hresult(windows::Win32::Foundation::E_ABORT));
        }
        let _ = SetTimer(
            Some(hwnd),
            REFRESH,
            if entry == Entry::Calendar { 30_000 } else { 1000 },
            None,
        );
        if entry != Entry::Calendar {
            let _ = PostMessageW(Some(launcher), WM_SOUND_REFRESH, WPARAM(hwnd.0 as usize), LPARAM(1));
        }
        let on = panel(hwnd).is_some_and(|cell| cell.borrow().confirmed_wifi_on);
        if let Some(request) = page_request(entry.page(), on) {
            if let Some(cell) = panel(hwnd) {
                cell.borrow_mut().queue.push_back(Action::Hardware(request));
            }
            let _ = PostMessageW(Some(launcher), WM_SOUND_ACTION, WPARAM(hwnd.0 as usize), LPARAM(0));
        }
        Ok(hwnd)
    }
}

pub fn apply_theme(
    hwnd: HWND,
    dark: bool,
    backdrop: crate::config::Backdrop,
    color: &str,
    options: crate::theme::MaterialOptions,
) {
    let Some(cell) = panel(hwnd) else { return };
    let Some(id) = identity(hwnd) else { return };
    let (revision, old) = {
        let Ok(mut p) = cell.try_borrow_mut() else { return };
        p.material_revision = p.material_revision.wrapping_add(1);
        p.foreground_prepared = false;
        p.frame_presented = false;
        (p.material_revision, p.material.take())
    };
    // Both native hosts are siblings under the stable dock owner.
    if let Some(old) = old {
        old.hide();
        drop(old);
    }
    let owner = unsafe { GetWindow(hwnd, GW_OWNER) }.ok();
    let material = crate::theme::SurfaceMaterial::prepare(owner, hwnd, dark, backdrop, color, options);
    if !is_current(id) || !state_is_current(hwnd, &cell) {
        return;
    }
    let mut p = cell.borrow_mut();
    if p.material_revision != revision {
        return;
    }
    p.palette = material.palette.clone();
    p.material = Some(material);
    drop(p);
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

fn restore_material(expected: u64, current: u64, occupied: bool) -> bool {
    expected == current && !occupied
}

/// Move native material work outside the foreground state borrow. A nested theme
/// update or HWND replacement wins over the material temporarily removed here.
fn material_effect(hwnd: HWND, present: bool) -> bool {
    let Some(cell) = panel(hwnd) else { return true };
    let Some(id) = identity(hwnd) else { return true };
    let (material, revision, dpi, opacity, radius) = {
        let Ok(mut p) = cell.try_borrow_mut() else { return true };
        let opacity = if p.remote {
            p.remote_alpha() as f32 / 255.
        } else {
            p.surface.as_ref().map_or(1., |s| s.progress.value.clamp(0., 1.))
        };
        let radius = 18. * p.surface.as_ref().map_or(1., SurfaceMotion::scale);
        (p.material.take(), p.material_revision, p.dpi, opacity, radius)
    };
    let Some(mut material) = material else { return true };
    let mut bounds = RECT::default();
    let okay = if present && unsafe { GetWindowRect(hwnd, &mut bounds) }.is_ok() {
        material.present(bounds, dpi, radius, opacity)
    } else {
        material.hide();
        true
    };
    if !is_current(id) || !state_is_current(hwnd, &cell) {
        return true;
    }
    let mut p = cell.borrow_mut();
    if !restore_material(revision, p.material_revision, p.material.is_some()) {
        return true;
    }
    if !okay {
        p.palette = material.palette.clone();
    }
    p.material = Some(material);
    drop(p);
    if !okay {
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
    okay
}

fn same_bounds(first: RECT, second: RECT) -> bool {
    first.left == second.left && first.top == second.top && first.right == second.right && first.bottom == second.bottom
}

/// Geometry is staged while handling input and applied after releasing Panel.
/// Hide the old clipping bounds before changing the foreground HWND dimensions.
fn flush_layout(hwnd: HWND) {
    let Some(cell) = panel(hwnd) else { return };
    let Some(id) = identity(hwnd) else { return };
    let bounds = cell.try_borrow_mut().ok().and_then(|mut p| p.pending_rect.take());
    if let Some(r) = bounds {
        let mut actual = RECT::default();
        if unsafe { GetWindowRect(hwnd, &mut actual) }.is_ok() && same_bounds(actual, r) {
            // Content/opacity updates still present normally. Rounded spring
            // positions need no native move or material hide/show cycle.
            return;
        }
        if let Ok(mut p) = cell.try_borrow_mut() {
            p.foreground_prepared = false;
            p.frame_presented = false;
        } else {
            return;
        }
        material_effect(hwnd, false);
        if !is_current(id) {
            return;
        }
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                None,
                r.left,
                r.top,
                (r.right - r.left).max(1),
                (r.bottom - r.top).max(1),
                SWP_NOACTIVATE | SWP_NOZORDER,
            );
        }
        if !is_current(id) {
            return;
        }
        if let Ok(p) = cell.try_borrow() {
            p.sync_input_bounds(hwnd);
        }
    }
}

fn publish_material_frame(hwnd: HWND) -> bool {
    let (presented, prepared) = panel(hwnd).map_or((false, false), |cell| {
        cell.try_borrow_mut().map_or((false, false), |mut p| {
            (std::mem::take(&mut p.frame_presented), p.foreground_prepared)
        })
    });
    if !prepared {
        material_effect(hwnd, false);
        return true;
    }
    // Activation may reorder a completed menu and its companion without a new
    // paint. Revalidate only a visible, clean foreground; pending frames must
    // still publish successfully before their new geometry/opacity is exposed.
    let clean_visible =
        prepared && unsafe { IsWindowVisible(hwnd).as_bool() && !GetUpdateRect(hwnd, None, false).as_bool() };
    !material_frame_ready(presented, prepared, clean_visible) || material_effect(hwnd, true)
}

fn material_frame_ready(presented: bool, prepared: bool, clean_visible: bool) -> bool {
    prepared && (presented || clean_visible)
}

fn scan_after_confirmation(previous: bool, confirmed: bool, wifi_page: bool) -> bool {
    !previous && confirmed && wifi_page
}
pub fn confirm_wifi(hwnd: HWND, on: bool) {
    if let Some(cell) = panel(hwnd)
        && let Ok(mut p) = cell.try_borrow_mut()
    {
        let scan = scan_after_confirmation(p.confirmed_wifi_on, on, p.page == Page::Wifi);
        p.confirmed_wifi_on = on;
        if scan {
            p.queue.push_back(Action::Hardware(control::Request::Networks));
            unsafe {
                let _ = PostMessageW(Some(p.launcher), WM_SOUND_ACTION, WPARAM(hwnd.0 as usize), LPARAM(0));
            }
        }
    }
}
pub fn update(hwnd: HWND, state: &State) {
    if let Some(cell) = panel(hwnd)
        && let Ok(mut p) = cell.try_borrow_mut()
        && p.page != Page::Calendar
        && p.dragging.is_none()
        && p.state != *state
    {
        p.state = state.clone();
        p.rebuild();
        p.fit(hwnd);
        if state_is_current(hwnd, &cell) {
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
        }
    }
    flush_layout(hwnd);
}

pub fn update_hardware(hwnd: HWND, hardware: control::Hardware) {
    if let Some(cell) = panel(hwnd)
        && let Ok(mut p) = cell.try_borrow_mut()
    {
        if p.page == Page::Calendar || p.dragging.is_some() || p.hardware == hardware {
            return;
        }
        p.hardware = hardware;
        p.rebuild();
        p.fit(hwnd);
        if state_is_current(hwnd, &cell) {
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
        }
    }
    flush_layout(hwnd);
}

pub fn take_action(hwnd: HWND) -> Option<Action> {
    let owner = panel(hwnd)?;
    let mut p = owner.try_borrow_mut().ok()?;
    p.queue.pop_front()
}

/// Normal dismissal animates. Owner teardown bypasses motion through a queued,
/// generation-bound destroy so no owner borrow crosses native destruction.
pub fn dismiss(id: Identity) {
    post_close(id, false);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // Destruction must retire the sibling before releasing foreground state.
    if matches!(msg, WM_DESTROY | WM_NCDESTROY) {
        material_effect(hwnd, false);
    }
    let result = unsafe { dispatch(hwnd, msg, wp, lp) };
    flush_layout(hwnd);
    publish_material_frame(hwnd);
    result
}

unsafe fn dispatch(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCALCSIZE && wp.0 != 0 {
            return LRESULT(0);
        }
        if msg == WM_NCACTIVATE {
            return DefWindowProcW(hwnd, msg, wp, LPARAM(-1));
        }
        if msg == WM_NCDESTROY {
            if let Some(id) = identity(hwnd) {
                retire_input(id);
                crate::logging::event("controls.destroyed", format_args!("serial={}", id.serial));
            }
            if let Ok(owner) = GetWindow(hwnd, GW_OWNER) {
                let _ = InvalidateRect(Some(owner), None, false);
            }
            let data = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<Panel>;
            let _ = RemovePropW(hwnd, IDENTITY);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            NativeState::release(data);
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let Some(cell) = panel(hwnd) else {
            return DefWindowProcW(hwnd, msg, wp, lp);
        };
        let Ok(mut p) = cell.try_borrow_mut() else {
            return DefWindowProcW(hwnd, msg, wp, lp);
        };
        if p.surface.as_ref().is_some_and(|surface| surface.closing)
            && matches!(
                msg,
                WM_LBUTTONDOWN | WM_LBUTTONUP | WM_MOUSEMOVE | WM_MOUSEWHEEL | WM_KEYDOWN | INPUT_ESCAPE
            )
        {
            return LRESULT(0);
        }
        if msg == WM_KEYDOWN && p.page == Page::Calendar {
            let focused = p.focus.and_then(|i| p.elements.get(i)).and_then(|e| match &e.click {
                Some(Click::Calendar(super::calendar::Action::Day(day))) => Some(*day),
                _ => None,
            });
            if let Some(day) = focused {
                p.calendar.focused = day;
            }

            let handled = match wp.0 as u16 {
                0x1b => return LRESULT(0),
                0x25 => {
                    p.calendar.move_days(-1);
                    true
                }
                0x27 => {
                    p.calendar.move_days(1);
                    true
                }
                0x26 => {
                    p.calendar.move_days(-7);
                    true
                }
                0x28 => {
                    p.calendar.move_days(7);
                    true
                }
                0x21 => {
                    p.calendar.apply(super::calendar::Action::Month(-1));
                    true
                }
                0x22 => {
                    p.calendar.apply(super::calendar::Action::Month(1));
                    true
                }
                0x24 => {
                    p.calendar.refresh();
                    p.calendar.apply(super::calendar::Action::Today);
                    true
                }
                _ => false,
            };
            if handled {
                p.rebuild();
                p.focus_calendar_day();
                p.fit(hwnd);
                let _ = InvalidateRect(Some(hwnd), None, false);
                return LRESULT(0);
            }
        }
        match msg {
            WM_CLOSE | DISMISS => {
                if msg == DISMISS && !same_identity(wp.0, GetPropW(hwnd, IDENTITY).0 as usize) {
                    return LRESULT(0);
                }
                p.input.take();
                if lp.0 == 1 || p.begin_close(hwnd) {
                    drop(p);
                    if state_is_current(hwnd, &cell) {
                        material_effect(hwnd, false);
                        if state_is_current(hwnd, &cell) {
                            let _ = DestroyWindow(hwnd);
                        }
                    }
                }
                return LRESULT(0);
            }
            WM_ERASEBKGND => return LRESULT(1),
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut paint);
                if let Err(error) = p.present(hwnd) {
                    if !p.paint_failed {
                        let serial = identity(hwnd).map_or(0, |id| id.serial);
                        crate::logging::event("controls.paint.failed", format_args!("serial={serial} error={error}"));
                        p.paint_failed = true;
                    }
                    request_close(hwnd, true);
                }
                if state_is_current(hwnd, &cell) {
                    let _ = EndPaint(hwnd, &paint);
                }
                return LRESULT(0);
            }
            INPUT_ESCAPE => {
                if !same_identity(wp.0, GetPropW(hwnd, IDENTITY).0 as usize) {
                    return LRESULT(0);
                }
                if escape_returns_home(p.page, lp.0 != 0) {
                    p.activate(Click::Page(Page::Home), hwnd, None);
                } else {
                    request_close(hwnd, false);
                }
            }
            WM_TIMER if wp.0 == PAGE_FADE => {
                if p.page_fade
                    .is_some_and(|start| start.elapsed().as_millis() >= u128::from(p.page_fade_ms))
                {
                    p.page_fade = None;
                    let _ = KillTimer(Some(hwnd), PAGE_FADE);
                }
                if p.remote {
                    if let Err(error) = p.present_remote_tick(hwnd) {
                        crate::logging::event("controls.remote.present.failed", format_args!("error={error}"));
                        request_close(hwnd, true);
                    }
                } else {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            WM_TIMER if wp.0 == SURFACE_MOTION => {
                let mut active = false;
                let mut finished = false;
                if let Some(surface) = &mut p.surface {
                    let now = Instant::now();
                    let dt = (now - surface.last).as_secs_f32();
                    surface.last = now;
                    active = surface.progress.tick(dt) | surface.height.tick(dt);
                    finished = surface.closing && !surface.progress.active();
                }
                p.place_surface(hwnd);
                if !active {
                    p.surface_running = false;
                    let _ = KillTimer(Some(hwnd), SURFACE_MOTION);
                }
                if finished {
                    drop(p);
                    if state_is_current(hwnd, &cell) {
                        material_effect(hwnd, false);
                        if state_is_current(hwnd, &cell) {
                            let _ = DestroyWindow(hwnd);
                        }
                    }
                    return LRESULT(0);
                }
            }
            WM_TIMER if wp.0 == REFRESH => {
                if p.page == Page::Calendar {
                    p.remember_calendar_focus();
                    if p.calendar.refresh() {
                        p.rebuild_calendar_focus();
                        p.fit(hwnd);
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    return LRESULT(0);
                }
                if p.page == Page::Home && p.elements.iter().any(|e| matches!(e.style, Style::Timeline(_, true))) {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                let _ = PostMessageW(Some(p.launcher), WM_SOUND_REFRESH, WPARAM(hwnd.0 as usize), LPARAM(0));
            }
            WM_ACTIVATE => {
                let active = wp.0 & 0xffff != WA_INACTIVE as usize;
                if let Some(id) = identity(hwnd) {
                    update_input(id, |target| target.focused = active);
                }
                if active {
                    return LRESULT(0);
                }
                let mut point = POINT::default();
                let on_opener = GetCursorPos(&mut point).is_ok()
                    && point.x >= p.opener.left
                    && point.x < p.opener.right
                    && point.y >= p.opener.top
                    && point.y < p.opener.bottom;
                if !on_opener {
                    request_close(hwnd, false);
                }
            }
            WM_SETTINGCHANGE if p.page == Page::Calendar => {
                p.remember_calendar_focus();
                p.calendar.refresh();
                p.rebuild_calendar_focus();
                p.fit(hwnd);
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            WM_MOUSEWHEEL => {
                if p.dragging.is_none() {
                    let delta = (wp.0 >> 16) as i16 as f32;
                    p.scroll = (p.scroll - delta * (42. / 120.)).clamp(0., p.max_scroll());
                }
            }
            WM_LBUTTONDOWN => {
                let (x, y) = p.pointer(lp);
                p.focus = p.elements.iter().position(|e| e.click.is_some() && e.bounds.hit(x, y));
                if let Some(i) = p.focus
                    && matches!(p.elements[i].style, Style::Slider(_) | Style::Timeline(..))
                    && (!matches!(p.elements[i].click, Some(Click::Volume)) || x > p.elements[i].bounds.x + 28.)
                {
                    p.dragging = Some((i, p.slider_value(i, x)));
                    let _ = SetCapture(hwnd);
                }
            }
            WM_MOUSEMOVE if p.dragging.is_some() => {
                let i = p.dragging.unwrap().0;
                p.dragging = Some((i, p.slider_value(i, p.pointer(lp).0)));
            }
            WM_LBUTTONUP => {
                let (x, y) = p.pointer(lp);
                if let Some((i, value)) = p.dragging.take() {
                    let _ = ReleaseCapture();
                    if matches!(p.elements[i].style, Style::Slider(_)) {
                        p.elements[i].style = Style::Slider(value);
                    }
                    if let Some(click) = p.elements[i].click.clone() {
                        p.activate(click, hwnd, Some(value));
                    }
                } else if let Some(i) = p.focus
                    && p.elements[i].bounds.hit(x, y)
                    && let Some(click) = p.elements[i].click.clone()
                {
                    p.activate(click, hwnd, None);
                }
            }
            WM_CAPTURECHANGED => {
                p.dragging = None;
                p.rebuild();
            }
            WM_KEYDOWN => match wp.0 as u16 {
                // The pass-through observer posts one latched Escape action;
                // native delivery and auto-repeat must not apply it again.
                0x1b => return LRESULT(0),
                0x09 | 0x26 | 0x28 => {
                    let count = p.elements.len();
                    let back = wp.0 == 0x26
                        || wp.0 == 0x09 && windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState(0x10) < 0;
                    let start = p.focus.unwrap_or(if back { 0 } else { count.saturating_sub(1) });
                    p.focus = (1..=count)
                        .map(|step| (start + if back { count - step } else { step }) % count)
                        .find(|&i| p.elements[i].click.is_some());
                    if let Some(i) = p.focus {
                        let b = p.elements[i].bounds;
                        if b.y < p.scroll {
                            p.scroll = b.y;
                        }
                        if b.y + b.h > p.scroll + p.height {
                            p.scroll = (b.y + b.h - p.height).min(p.max_scroll());
                        }
                    }
                }
                0x0d | 0x20 | 0x25 | 0x27 => {
                    if let Some(i) = p.focus
                        && let Some(click) = p.elements[i].click.clone()
                    {
                        let value = match (wp.0, &p.elements[i].style) {
                            (key @ (0x25 | 0x27), Style::Timeline(timeline, playing)) => {
                                let fraction = (timeline.position(*playing) - timeline.start) as f64
                                    / (timeline.end - timeline.start) as f64;
                                let step = if key == 0x25 { -5. } else { 5. };
                                Some((fraction * 100. + step).clamp(0., 100.).round() as u32)
                            }
                            (0x25, Style::Slider(v)) => Some(v.saturating_sub(5)),
                            (0x27, Style::Slider(v)) => Some((v + 5).min(100)),
                            _ => None,
                        };
                        if !matches!(wp.0, 0x25 | 0x27) || value.is_some() {
                            p.activate(click, hwnd, value);
                        }
                    }
                }
                _ => {}
            },
            _ => return after_borrow(p, || DefWindowProcW(hwnd, msg, wp, lp)),
        }
        if state_is_current(hwnd, &cell) && !matches!(msg, WM_TIMER | WM_ACTIVATE) {
            p.fit(hwnd);
            if state_is_current(hwnd, &cell) {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
        }
        LRESULT(0)
    }
}

#[cfg(test)]
fn qa_panel(entry: Entry, width: f32) -> windows::core::Result<Panel> {
    let mut state = State {
        wifi: Some(Some(crate::commands::wifi::State {
            on: true,
            network: Some("BAIK-Dev".into()),
        })),
        audio: Some(Some(crate::commands::audio::State {
            volume: 64,
            muted: false,
            device: "L80 PRO".into(),
        })),
        ..Default::default()
    };
    state.bluetooth = Some(Some(crate::commands::bluetooth::State {
        on: true,
        connected: vec!["L80 PRO".into()],
    }));
    state.outputs = vec![
        crate::commands::audio::Output {
            id: "headphones".into(),
            name: "L80 PRO".into(),
            default: true,
        },
        crate::commands::audio::Output {
            id: "speakers".into(),
            name: "Desktop speakers".into(),
            default: false,
        },
    ];
    let networks = (0..21)
        .map(|index| {
            let name = if index == 0 {
                "BAIK-Dev".into()
            } else {
                format!(
                    "{} {}",
                    if index % 3 == 0 {
                        "Office wireless network"
                    } else {
                        "Studio"
                    },
                    index
                )
            };
            crate::commands::wifi::Network {
                name: name.clone(),
                profile: Some(name),
                connected: index == 0,
                secured: true,
                signal: Some(80),
            }
        })
        .collect();
    let hardware = control::Hardware {
        batteries: vec![("L80 PRO".into(), Some(72))],
        network_scan: crate::commands::wifi::ScanStatus::Ready,
        networks: Some(crate::commands::wifi::Networks {
            list: networks,
            location_off: false,
        }),
        ..Default::default()
    };
    let mut calendar = super::calendar::Calendar::new();
    calendar.today = super::calendar_model::Date::new(2026, 11, 12).unwrap();
    calendar.selected = super::calendar_model::Date::new(2026, 11, 17).unwrap();
    calendar.focused = calendar.selected;
    calendar.month = calendar.today.month();
    let mut p = Panel {
        launcher: HWND::default(),
        dpi: 96,
        width,
        height: 700.,
        max_height: 700.,
        bottom: 0,
        below: true,
        confirmed_wifi_on: true,
        state,
        hardware,
        renderer: Renderer::new()?,
        palette: Palette::new(true, false),
        page: entry.page(),
        elements: Vec::new(),
        queue: VecDeque::new(),
        hidden: 0,
        scroll: 0.,
        focus: None,
        dragging: None,
        remote: false,
        remote_motion_ms: 0,
        page_fade_ms: 0,
        page_fade: None,
        surface: None,
        surface_running: false,
        calendar,
        input: None,
        opener: RECT::default(),
        paint_failed: false,
        material: None,
        material_revision: 0,
        pending_rect: None,
        frame_presented: false,
        foreground_prepared: false,
    };
    p.rebuild();
    Ok(p)
}

#[cfg(test)]
pub(crate) fn qa_elements(entry: Entry, width: f32) -> windows::core::Result<(Vec<Element>, f32)> {
    let p = qa_panel(entry, width)?;
    let height = p.content_height();
    Ok((p.elements, height))
}

#[cfg(test)]
pub(crate) fn qa_compact_elements(scenario: &str) -> windows::core::Result<(Vec<Element>, f32)> {
    let mut p = qa_panel(Entry::Home, WIDTH)?;
    match scenario {
        "no-sound" => p.state.audio = Some(None),
        "hidden" => p.hidden = 15,
        "long-labels" => {
            p.state.wifi.as_mut().unwrap().as_mut().unwrap().network =
                Some("Office wireless network on the fourth floor".into());
            p.state.audio.as_mut().unwrap().as_mut().unwrap().device =
                "Headphones with a particularly long device name".into();
        }
        "media" => {
            p.state.media = Some(Some(media::State {
                source: "fixture".into(),
                title: "Midnight City".into(),
                artist: "M83".into(),
                app: "Music".into(),
                playing: true,
                timeline: Some(media::Timeline {
                    start: 0,
                    end: 2400000000,
                    position: 800000000,
                    updated: 0,
                    rate: 1.,
                    seek: Some((0, 2400000000)),
                }),
            }))
        }
        _ => {}
    }
    p.rebuild();
    let height = p.content_height();
    Ok((p.elements, height))
}

#[cfg(test)]
pub(crate) fn qa_calendar_elements(
    browse: i32,
    selection: Option<super::calendar_model::Date>,
) -> windows::core::Result<(Vec<Element>, f32)> {
    let mut p = qa_panel(Entry::Calendar, 392.)?;
    p.calendar.apply(super::calendar::Action::Today);
    p.calendar.apply(super::calendar::Action::Month(browse));
    if let Some(day) = selection {
        p.calendar.apply(super::calendar::Action::Day(day));
    }
    p.rebuild();
    let height = p.content_height();
    Ok((p.elements, height))
}

#[cfg(test)]
mod entry_tests {
    use super::*;
    #[test]
    fn companion_revalidation_requires_a_valid_foreground_after_resize_theme_or_failed_paint() {
        assert!(
            material_frame_ready(true, true, false),
            "first complete frame may prepare before show"
        );
        assert!(
            material_frame_ready(false, true, true),
            "clean visible frame can repair activation-only reorder"
        );
        assert!(
            !material_frame_ready(false, true, false),
            "pending paint must not publish unpainted motion"
        );
        assert!(
            !material_frame_ready(false, false, true),
            "old geometry/theme is no longer prepared"
        );
        assert!(
            !material_frame_ready(true, false, true),
            "invalidated preparation wins over an older frame flag"
        );
    }
    #[test]
    fn compact_home_keeps_complete_action_inventory_and_separate_hit_targets() -> Result<()> {
        let p = qa_panel(Entry::Home, WIDTH)?;
        assert_eq!(p.content_height(), 344.);
        assert_eq!(
            p.elements.iter().filter(|e| matches!(e.style, Style::Tile(_))).count(),
            4
        );
        for page in [
            Page::Wifi,
            Page::Bluetooth,
            Page::Sound,
            Page::Output,
            Page::Mixer,
            Page::Display,
            Page::Edit,
        ] {
            assert!(
                p.elements
                    .iter()
                    .any(|e| matches!(e.click, Some(Click::Page(found)) if found == page))
            );
        }
        assert!(
            p.elements
                .iter()
                .any(|e| matches!(e.click, Some(Click::Action(Action::Project))))
        );
        assert!(
            p.elements
                .iter()
                .any(|e| matches!(e.click, Some(Click::Action(Action::QuickSettings))))
        );
        assert!(
            p.elements
                .iter()
                .any(|e| matches!(e.click, Some(Click::Action(Action::WispSettings))))
        );
        assert!(p.elements.iter().any(|e| matches!(
            e.click,
            Some(Click::Action(Action::Settings("ms-settings:notifications")))
        )));
        let actions = p.elements.iter().filter(|e| e.is_interactive()).collect::<Vec<_>>();
        for (i, first) in actions.iter().enumerate() {
            for second in &actions[i + 1..] {
                let a = first.bounds;
                let b = second.bounds;
                assert!(a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y);
            }
        }
        Ok(())
    }
    #[test]
    fn compact_volume_track_mapping_keeps_mute_and_endpoints_distinct() -> Result<()> {
        let p = qa_panel(Entry::Home, WIDTH)?;
        let index = p
            .elements
            .iter()
            .position(|e| matches!(e.click, Some(Click::Volume)))
            .unwrap();
        let b = p.elements[index].bounds;
        assert!(b.hit(b.x + 14., b.y + 16.));
        for percent in [0, 25, 50, 75, 100] {
            let x = b.x + 36. + (b.w - 44.) * percent as f32 / 100.;
            assert_eq!(p.slider_value(index, x), percent);
            assert!(x > b.x + 28.);
        }
        assert_eq!(p.slider_value(index, b.x), 0);
        assert_eq!(p.slider_value(index, b.x + b.w), 100);
        Ok(())
    }
    #[test]
    fn compact_optional_content_and_all_pages_preserve_geometry_and_media_actions() -> Result<()> {
        for scenario in ["default", "no-sound", "hidden", "long-labels", "media"] {
            let (elements, height) = qa_compact_elements(scenario)?;
            assert!(height <= if scenario == "media" { 500. } else { 344. });
            assert!(
                elements
                    .iter()
                    .all(|e| e.bounds.x >= 0. && e.bounds.x + e.bounds.w <= WIDTH)
            );
            if scenario == "media" {
                assert_eq!(
                    elements
                        .iter()
                        .filter(|e| matches!(e.click, Some(Click::Action(Action::Run(Command::Media(_))))))
                        .count(),
                    3
                );
                assert!(elements.iter().any(|e| matches!(e.click, Some(Click::Seek(_)))));
            }
            if scenario == "hidden" {
                assert!(
                    !elements
                        .iter()
                        .any(|e| matches!(e.click, Some(Click::Page(Page::Display | Page::Sound))))
                );
            }
        }
        let mut p = qa_panel(Entry::Home, WIDTH)?;
        for page in [
            Page::Sound,
            Page::Wifi,
            Page::Bluetooth,
            Page::Output,
            Page::Mixer,
            Page::Display,
            Page::Edit,
            Page::Calendar,
        ] {
            p.page = page;
            p.rebuild();
            assert!(
                p.elements
                    .iter()
                    .all(|e| e.bounds.w > 0. && e.bounds.x + e.bounds.w <= WIDTH)
            );
            assert!(p.content_height().is_finite());
        }
        Ok(())
    }
    #[test]
    fn stale_material_effect_cannot_overwrite_reentrant_theme_or_replacement_host() {
        assert!(restore_material(7, 7, false));
        assert!(!restore_material(7, 8, false));
        assert!(!restore_material(7, 7, true));
    }
    #[test]
    fn failed_first_presentation_cleans_up_without_showing() {
        let events = RefCell::new(Vec::new());
        let result = show_prepared(
            Err("presentation failed"),
            || events.borrow_mut().push("show"),
            || events.borrow_mut().push("destroy"),
        );
        assert_eq!(result, Err("presentation failed"));
        assert_eq!(*events.borrow(), ["destroy"]);
        events.borrow_mut().clear();
        let prepared = {
            events.borrow_mut().push("pixels");
            Ok::<(), &str>(())
        };
        show_prepared(
            prepared,
            || events.borrow_mut().push("show"),
            || events.borrow_mut().push("destroy"),
        )
        .unwrap();
        assert_eq!(*events.borrow(), ["pixels", "show"]);
    }
    #[test]
    fn complete_outside_press_is_retained_without_waiting_for_idle_dispatch() {
        let mut input = InputGuard::default(); // opened on button-up
        let mut pending = Vec::new();
        if input.button(1, true, false, false) {
            pending.push("close");
        }
        assert!(!input.button(1, false, false, false));
        assert_eq!(pending, ["close"]); // down/up both precede UI dispatch
    }
    #[test]
    fn only_the_actual_opening_gesture_is_suppressed() {
        let mut input = InputGuard {
            buttons: 1,
            escape: false,
        };
        assert!(!input.button(1, true, false, false));
        assert!(!input.button(1, false, false, false));
        assert!(input.button(1, true, false, false)); // next press needs no idle tick
        assert!(!input.button(1, false, false, false));
        assert!(!input.button(1, true, false, true)); // owner's toggle
    }
    #[test]
    fn escape_edges_keep_focus_navigation_and_fallback_separate() {
        let mut input = InputGuard::default();
        let mut pending = Vec::new();
        if input.escape(true) {
            pending.push((Page::Sound, true));
        }
        assert!(!input.escape(true)); // repeat/native delivery cannot enqueue again
        assert!(!input.escape(false));
        assert_eq!(pending.len(), 1); // complete tap retained before dispatch
        assert!(escape_returns_home(pending[0].0, pending[0].1));
        assert!(!escape_returns_home(Page::Home, true));
        assert!(!escape_returns_home(Page::Calendar, true));
        assert!(!escape_returns_home(Page::Sound, false)); // denied foreground closes
        assert!(input.escape(true));
    }
    #[test]
    fn old_observer_teardown_cannot_unregister_replacement() {
        let old = Identity {
            hwnd: HWND(1usize as _),
            serial: 41,
        };
        let new = Identity {
            hwnd: old.hwnd,
            serial: 42,
        }; // reused HWND
        INPUT_TARGET.with(|slot| {
            slot.set(Some(InputTarget {
                id: new,
                bounds: RECT::default(),
                opener: RECT::default(),
                focused: false,
                guard: InputGuard::default(),
            }))
        });
        retire_input(old);
        update_input(old, |target| target.focused = true);
        assert!(INPUT_TARGET.with(|slot| slot.get().is_some_and(|target| target.id == new && !target.focused)));
        let moved_opener = RECT {
            left: -250,
            top: -150,
            right: -200,
            bottom: -100,
        };
        update_input(new, |target| {
            target.opener = moved_opener;
            target.focused = true;
        });
        assert!(INPUT_TARGET.with(|slot| {
            slot.get()
                .is_some_and(|target| target.focused && contains(target.opener, POINT { x: -225, y: -125 }))
        }));
        assert!(!same_identity(old.serial, new.serial)); // queued close/escape rejected
        retire_input(new);
        assert!(INPUT_TARGET.with(|slot| slot.get().is_none()));
    }
    #[test]
    fn failed_native_state_install_releases_the_unpublished_owner() {
        struct Counted(Rc<std::cell::Cell<usize>>);
        impl Drop for Counted {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let drops = Rc::new(std::cell::Cell::new(0));
        let result = NativeState::install(Counted(drops.clone()), |pointer| {
            assert!(!pointer.is_null());
            Err::<(), _>("userdata registration failed")
        });
        assert!(result.is_err());
        assert_eq!(drops.get(), 1);
    }
    #[test]
    fn native_state_survives_nested_destroy_while_callback_or_public_entry_borrows() {
        struct Counted {
            drops: Rc<std::cell::Cell<usize>>,
            value: usize,
        }
        impl Drop for Counted {
            fn drop(&mut self) {
                self.drops.set(self.drops.get() + 1);
            }
        }
        let drops = Rc::new(std::cell::Cell::new(0));
        let native = std::cell::Cell::new(NativeState::into_raw(Counted {
            drops: drops.clone(),
            value: 0,
        }));
        let outer = unsafe { NativeState::acquire(native.get()) }.unwrap();
        let mut borrowed = outer.borrow_mut();
        let nested = unsafe { NativeState::acquire(native.get()) }.unwrap();
        assert!(nested.try_borrow_mut().is_err());
        unsafe {
            NativeState::release(native.replace(std::ptr::null()));
        }
        assert_eq!(drops.get(), 0);
        borrowed.value = 42;
        drop(nested);
        assert_eq!(drops.get(), 0);
        drop(borrowed);
        assert_eq!(outer.borrow().value, 42);
        drop(outer);
        assert_eq!(drops.get(), 1);
        // A repeated native destruction observes cleared userdata and is a no-op.
        unsafe {
            NativeState::release(native.replace(std::ptr::null()));
        }
        assert_eq!(drops.get(), 1);
    }
    #[test]
    fn default_dispatch_releases_mutable_borrow_before_nested_close() {
        let native = std::cell::Cell::new(NativeState::into_raw(7usize));
        let callback = unsafe { NativeState::acquire(native.get()) }.unwrap();
        let weak = Rc::downgrade(&callback);
        let result = after_borrow(callback.borrow_mut(), || {
            let nested = unsafe { NativeState::acquire(native.get()) }.unwrap();
            *nested.borrow_mut() = 9;
            unsafe {
                NativeState::release(native.replace(std::ptr::null()));
            }
            assert!(weak.upgrade().is_some());
            12
        });
        assert_eq!(result, 12);
        assert_eq!(*callback.borrow(), 9);
        drop(callback);
        assert!(weak.upgrade().is_none());
    }
    #[test]
    fn month_button_activation_preserves_button_focus_and_real_selection() -> windows::core::Result<()> {
        let mut p = qa_panel(Entry::Calendar, 392.)?;
        p.calendar.apply(super::super::calendar::Action::Today);
        let selected = p.calendar.selected;
        for _ in 0..22 {
            p.activate(
                Click::Calendar(super::super::calendar::Action::Month(-1)),
                HWND::default(),
                None,
            );
        }
        assert_eq!(p.calendar.selected, selected);
        assert_eq!(
            p.calendar.month,
            super::super::calendar_model::Month::new(2025, 1).unwrap()
        );
        assert!(matches!(
            p.elements[p.focus.unwrap()].click,
            Some(Click::Calendar(super::super::calendar::Action::Month(-1)))
        ));
        p.calendar.first_weekday = 6;
        p.rebuild_calendar_focus();
        assert!(matches!(
            p.elements[p.focus.unwrap()].click,
            Some(Click::Calendar(super::super::calendar::Action::Month(-1)))
        ));
        p.calendar.move_days(1);
        p.rebuild();
        p.focus_calendar_day();
        assert!(
            matches!(p.elements[p.focus.unwrap()].click,Some(Click::Calendar(super::super::calendar::Action::Day(day))) if day==p.calendar.focused)
        );
        assert_eq!(p.calendar.selected, selected);
        Ok(())
    }
    #[test]
    fn weekday_change_remaps_focused_date_and_enter_target() -> windows::core::Result<()> {
        let mut p = qa_panel(Entry::Calendar, 392.)?;
        p.calendar.first_weekday = 0;
        p.rebuild();
        p.focus_calendar_day();
        let date = p.calendar.focused;
        let old = p.focus.unwrap();
        p.remember_calendar_focus();
        p.calendar.first_weekday = 6;
        p.rebuild();
        p.focus_calendar_day();
        assert_ne!(p.focus.unwrap(), old);
        let Some(Click::Calendar(super::super::calendar::Action::Day(target))) = &p.elements[p.focus.unwrap()].click
        else {
            panic!("Enter must target a date");
        };
        assert_eq!(*target, date);
        assert_eq!(p.calendar.focused, date);
        Ok(())
    }
    #[test]
    fn historical_calendar_has_return_action_without_false_today_or_selection() -> windows::core::Result<()> {
        let (elements, _) = qa_calendar_elements(-22, None)?;
        assert!(elements.iter().any(|e| e.title == "Back to today"
            && matches!(e.click, Some(Click::Calendar(super::super::calendar::Action::Today)))));
        assert!(elements.iter().any(|e| e.title.starts_with("Selected: ")));
        assert!(!elements.iter().any(|e| matches!(
            e.style,
            Style::CalendarDay { today: true, .. } | Style::CalendarDay { selected: true, .. }
        )));
        let (elements, _) =
            qa_calendar_elements(-22, Some(super::super::calendar_model::Date::new(2025, 1, 8).unwrap()))?;
        assert_eq!(
            elements
                .iter()
                .filter(|e| matches!(
                    e.style,
                    Style::CalendarDay {
                        selected: true,
                        today: false,
                        ..
                    }
                ))
                .count(),
            1
        );
        Ok(())
    }
    #[test]
    fn sound_battery_requires_unambiguous_connected_output_and_reported_percent() -> windows::core::Result<()> {
        let mut p = qa_panel(Entry::Sound, 424.)?;
        assert_eq!(p.output_battery("Headphones (L80 PRO)"), Some(72));
        assert_eq!(p.output_battery("Speakers (Other device)"), None);
        p.rebuild();
        assert!(
            p.elements
                .iter()
                .any(|e| matches!(e.style, Style::CurrentRow) && e.detail.contains("Battery 72%"))
        );
        p.hardware.batteries.push(("L80 PRO".into(), Some(50)));
        assert_eq!(p.output_battery("L80 PRO"), None);
        p.hardware.batteries = vec![("L80 PRO".into(), None)];
        assert_eq!(p.output_battery("L80 PRO"), None);
        p.hardware.batteries = vec![("L80 PRO".into(), Some(72))];
        p.state.bluetooth = None;
        assert_eq!(p.output_battery("L80 PRO"), None);
        Ok(())
    }
    #[test]
    fn pending_and_failed_discovery_never_claims_an_empty_success() {
        use crate::commands::wifi::ScanStatus;
        assert_eq!(network_empty_message(&ScanStatus::Ready), "No other networks found");
        for state in [
            ScanStatus::Idle,
            ScanStatus::Scanning,
            ScanStatus::Failed("timeout".into()),
        ] {
            assert!(!network_empty_message(&state).contains("No other"));
        }
    }
    #[test]
    fn wifi_viewport_cap_follows_page_navigation_without_changing_monitor_limit() {
        assert_eq!(viewport_height(Page::Home, 900., 700.), 700.);
        assert_eq!(viewport_height(Page::Wifi, 900., 700.), 480.);
        assert_eq!(viewport_height(Page::Wifi, 900., 320.), 320.);
        assert_eq!(viewport_height(Page::Sound, 900., 700.), 700.);
    }
    #[test]
    fn discovery_entry_requires_known_enabled_adapter() {
        for wifi in [
            None,
            Some(None),
            Some(Some(crate::commands::wifi::State {
                on: false,
                network: None,
            })),
            Some(Some(crate::commands::wifi::State {
                on: true,
                network: None,
            })),
        ] {
            let on = wifi.as_ref().and_then(Option::as_ref).is_some_and(|w| w.on);
            assert_eq!(page_request(Page::Wifi, on).is_some(), on);
        }
        assert!(page_request(Page::Display, false).is_some());
        assert!(page_request(Page::Calendar, true).is_none());
    }
    #[test]
    fn scans_require_confirmed_success_and_visible_wifi_page() {
        assert!(
            !scan_after_confirmation(false, false, true),
            "optimistic on followed by failed radio-on must not scan"
        );
        assert!(scan_after_confirmation(false, true, true));
        assert!(!scan_after_confirmation(false, true, false));
        assert!(!scan_after_confirmation(true, true, true));
    }
    #[test]
    fn current_connection_wins_over_cached_network_flags() {
        let network = |name: &str, connected| crate::commands::wifi::Network {
            name: name.into(),
            connected,
            profile: Some(name.into()),
            secured: true,
            signal: Some(80),
        };
        let old = network("A", true);
        let current = network("B", false);
        assert!(is_available_network(&old, Some("B")));
        assert!(!is_available_network(&current, Some("B")));
        assert!(is_available_network(&old, None));
        assert!(is_available_network(&current, None));
    }
    #[test]
    fn constrained_calendar_keeps_keyboard_selection_visible() {
        let last = Bounds {
            x: 20.,
            y: 360.,
            w: 48.,
            h: 40.,
        };
        let first = Bounds { y: 140., ..last };
        let scroll = visible_scroll(0., 260., 202., last);
        assert_eq!(scroll, 140.);
        assert!(last.y >= scroll && last.y + last.h <= scroll + 260.);
        let scroll = visible_scroll(200., 260., 202., first);
        assert_eq!(scroll, 140.);
        assert!(first.y >= scroll && first.y + first.h <= scroll + 260.);
    }
    #[test]
    fn initial_calendar_selection_fits_a_short_work_area() -> windows::core::Result<()> {
        let (elements, content_height) = qa_elements(Entry::Calendar, 392.)?;
        let selected = elements
            .iter()
            .find(|e| matches!(e.style, Style::CalendarDay { selected: true, .. }))
            .unwrap();
        let height = 180.;
        let scroll = visible_scroll(0., height, content_height - height, selected.bounds);
        assert!(scroll > 0.);
        assert!(selected.bounds.y >= scroll && selected.bounds.y + selected.bounds.h <= scroll + height);
        Ok(())
    }
    #[test]
    fn calendar_grid_headers_do_not_select_dates_or_request_hardware() -> windows::core::Result<()> {
        let (elements, _) = qa_elements(Entry::Calendar, 392.)?;
        let day_headers = elements
            .iter()
            .filter(|e| matches!(e.style, Style::CalendarDay { .. }) && e.click.is_none())
            .count();
        assert_eq!(day_headers, 7);
        assert_eq!(
            elements
                .iter()
                .filter(|e| matches!(e.click, Some(Click::Calendar(super::super::calendar::Action::Day(_)))))
                .count(),
            42
        );
        assert!(
            elements
                .iter()
                .filter_map(|e| e.click.as_ref())
                .all(|click| matches!(click, Click::Calendar(_)))
        );
        assert!(Entry::Calendar.page().hardware_request().is_none());
        Ok(())
    }
    #[test]
    fn wifi_grid_keeps_saved_profile_actions_and_two_columns() -> windows::core::Result<()> {
        let (elements, _) = qa_elements(Entry::Wifi, 424.)?;
        let cards = elements
            .iter()
            .filter(|e| matches!(e.style, Style::CompactCard(_)))
            .collect::<Vec<_>>();
        assert_eq!(cards.len(), 15);
        assert!(cards.iter().all(|e| e.bounds.h == 56.));
        assert!(elements.iter().any(|e| matches!(e.click, Some(Click::RefreshNetworks))));
        assert_eq!(cards[0].bounds.y, cards[1].bounds.y);
        assert!(cards[1].bounds.x > cards[0].bounds.x);
        assert!(cards.iter().all(|e|matches!(&e.click,Some(Click::Action(Action::Run(Command::WifiConnect(profile)))) if !profile.is_empty())));
        Ok(())
    }
    fn motion() -> SurfaceMotion {
        let mut progress = crate::animation::Motion::default();
        progress.retarget_duration(true, 180);
        SurfaceMotion {
            progress,
            duration_ms: 180,
            height: super::super::topbar::model::Spring::new(400.),
            last: Instant::now(),
            final_rect: RECT {
                left: 100,
                top: 40,
                right: 524,
                bottom: 440,
            },
            anchor_x: 500,
            closing: false,
        }
    }
    #[test]
    fn popup_motion_keeps_size_and_input_positions_stable_while_rising() {
        let mut s = motion();
        let initial = s.rect();
        assert_eq!(initial.right - initial.left, 424);
        assert!(initial.top < 40);
        let logical = (100., 80.);
        let presented = (logical.0 * s.scale(), logical.1 * s.scale());
        assert!((presented.0 / s.scale() - logical.0).abs() < 0.001);
        assert!((presented.1 / s.scale() - logical.1).abs() < 0.001);
        for _ in 0..100 {
            s.progress.tick(0.01);
        }
        assert_eq!(s.rect(), s.final_rect);
        let position = s.progress.value;
        s.progress.retarget_duration(false, s.duration_ms);
        assert_eq!(s.progress.value, position);
        for _ in 0..100 {
            s.progress.tick(0.01);
        }
        assert_eq!(s.rect().right - s.rect().left, 424);
    }
    #[test]
    fn popup_progress_honors_requested_duration_reversal_zero_and_stalled_frames() {
        let mut surface = motion();
        assert!(surface.progress.tick(0.09));
        let midway = surface.progress.value;
        surface.progress.retarget_duration(false, surface.duration_ms);
        assert_eq!(surface.progress.value, midway);
        assert!(surface.progress.tick(0.03));
        surface.progress.retarget_duration(true, surface.duration_ms);
        assert!(!surface.progress.tick(0.5));
        assert_eq!(surface.progress.value, 1.);
        surface.progress.retarget_duration(false, 0);
        assert_eq!(surface.progress.value, 0.);
        assert!(!surface.progress.active());
        surface.progress.retarget_duration(true, 180);
        assert!(surface.progress.tick(0.179));
        assert!(!surface.progress.tick(0.002));
        assert_eq!(surface.progress.value, 1.);
    }
    #[test]
    fn repeated_subpixel_motion_preserves_physical_bounds_until_a_pixel_changes() {
        let mut surface = motion();
        let actual = surface.rect();
        assert!(same_bounds(actual, surface.rect()));
        surface.progress.value += 0.00001;
        surface.height.value += 0.00001;
        assert!(
            same_bounds(actual, surface.rect()),
            "subpixel motion must not hide and reposition the native material"
        );
        surface.final_rect.left += 1;
        surface.final_rect.right += 1;
        assert!(
            !same_bounds(actual, surface.rect()),
            "a physical pixel move still updates both windows"
        );
        let mut resized = actual;
        resized.bottom += 1;
        assert!(!same_bounds(actual, resized));
    }
    #[test]
    fn async_height_retarget_preserves_current_presented_height() {
        let mut s = motion();
        s.progress.settle();
        let before = s.rect();
        s.height.target = 700.;
        s.final_rect.bottom = 740;
        assert_eq!(s.rect(), before);
        let later = s.last + std::time::Duration::from_secs(10);
        s.restart(later);
        let elapsed = (later + std::time::Duration::from_millis(10) - s.last).as_secs_f32();
        s.height.tick(elapsed);
        assert!(s.height.value > 400. && s.height.value < 420.);
        s.settle();
        assert_eq!(s.height.value, 700.);
        assert_eq!(s.rect().bottom, 740);
    }
    #[test]
    fn missing_motion_timer_settles_to_a_visible_final_surface() {
        let mut s = motion();
        s.settle();
        assert_eq!(s.progress.value, 1.);
        assert_eq!(s.rect(), s.final_rect);
        s.closing = true;
        s.progress.retarget_duration(false, s.duration_ms);
        s.settle();
        assert_eq!(s.progress.value, 0.);
    }
    #[test]
    fn retired_popup_identity_cannot_destroy_a_reused_handle() {
        assert!(same_identity(12, 12));
        assert!(!same_identity(12, 13));
        assert!(!same_identity(12, 0));
        assert!(!same_identity(0, 0));
    }
    #[test]
    fn direct_wifi_entry_requests_current_networks() {
        assert!(matches!(
            Entry::Wifi.page().hardware_request(),
            Some(control::Request::Networks)
        ));
        assert!(matches!(
            Entry::Sound.page().hardware_request(),
            Some(control::Request::Batteries)
        ));
        assert!(Entry::Home.page().hardware_request().is_none());
    }
}
