//! A native, nonactivating music player on the primary display.

use std::{
    cell::{Cell, RefCell},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{HiDpi::GetDpiForWindow, WindowsAndMessaging::*},
    },
    core::{Error, Result, w},
};

use crate::{
    commands::media::{Control, NotchTrack},
    dock::render::Renderer,
    theme::Palette,
};

pub const REFRESH: u32 = WM_APP + 33;
pub const ACTION: u32 = WM_APP + 34;
const POLL: usize = 1;
const ANIMATE: usize = 2;
const COLLAPSE: u32 = WM_APP + 1;

thread_local! { static EXPANDED_NOTCH: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) }; }
const POLL_MS: u32 = 120;
const COMPACT: (f32, f32) = (190., 38.);
const EXPANDED: (f32, f32) = (354., 178.);

pub struct Notch {
    hwnd: HWND,
    state: Box<RefCell<State>>,
}

struct State {
    owner: HWND,
    renderer: Renderer,
    palette: Palette,
    track: Option<NotchTrack>,
    visible: bool,
    expanded: bool,
    progress: f32,
    motion: Option<(Instant, f32)>,
    last_request: Instant,
    fullscreen_at: Option<Instant>,
    action: Option<Control>,
    geometry: Option<(i32, i32, i32, i32, u32)>,
    outside_hook: Option<HHOOK>,
    hover: Option<(f32, f32)>,
}

impl Notch {
    pub fn new(owner: HWND) -> Result<Self> {
        let mut state = Box::new(RefCell::new(State {
            owner,
            renderer: Renderer::new()?,
            palette: Palette::new(true, false),
            track: None,
            visible: false,
            expanded: false,
            progress: 0.,
            motion: None,
            last_request: Instant::now(),
            fullscreen_at: None,
            action: None,
            geometry: None,
            outside_hook: None,
            hover: None,
        }));
        unsafe {
            let instance = GetModuleHandleW(None)?.into();
            let class = WNDCLASSW {
                hInstance: instance,
                lpfnWndProc: Some(wndproc),
                lpszClassName: w!("Wisp.MusicNotch"),
                hCursor: LoadCursorW(None, IDC_HAND)?,
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
                return Err(Error::from_thread());
            }
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
                class.lpszClassName,
                w!("Wisp music notch"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(instance),
                Some((&raw mut *state).cast()),
            )?;
            let notch = Self { hwnd, state };
            if SetTimer(Some(hwnd), POLL, POLL_MS, None) == 0 {
                return Err(Error::from_thread());
            }
            notch.state.borrow_mut().request(hwnd);
            Ok(notch)
        }
    }

    pub fn apply_theme(
        &self,
        dark: bool,
        backdrop: crate::config::Backdrop,
        color: &str,
        options: crate::theme::MaterialOptions,
    ) {
        let palette = crate::theme::apply_backdrop(self.hwnd, dark, backdrop, true, color, options);
        self.state.borrow_mut().palette = palette;
        let _ = unsafe { InvalidateRect(Some(self.hwnd), None, false) };
    }

    pub fn matches(&self, id: usize) -> bool {
        self.hwnd.0 as usize == id
    }

    pub fn update(&mut self, track: Option<NotchTrack>) {
        let mut state = self.state.borrow_mut();
        state.track = track;
        state.sync_visibility(self.hwnd);
    }

    pub fn take_action(&mut self) -> Option<Control> {
        self.state.borrow_mut().action.take()
    }

    pub fn set_media_exclusions(&mut self, exclusions: &[String]) {
        let mut state = self.state.borrow_mut();
        if state
            .track
            .as_ref()
            .is_some_and(|track| crate::commands::media::excluded(&track.media.source, &track.media.app, exclusions))
        {
            state.track = None;
            state.action = None;
            state.sync_visibility(self.hwnd);
        }
    }
}

impl Drop for Notch {
    fn drop(&mut self) {
        self.state.borrow_mut().unhook_outside();
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

impl State {
    fn request(&mut self, hwnd: HWND) {
        self.last_request = Instant::now();
        let _ = unsafe { PostMessageW(Some(self.owner), REFRESH, WPARAM(hwnd.0 as usize), LPARAM(0)) };
    }

    fn sync_visibility(&mut self, hwnd: HWND) {
        let playing = self.track.as_ref().is_some_and(|track| track.media.playing);
        if (playing || self.visible) && crate::dock::primary_fullscreen() {
            self.fullscreen_at = Some(Instant::now());
        }
        let blocked = self
            .fullscreen_at
            .is_some_and(|at| at.elapsed() < Duration::from_millis(350));
        let show = self.track.is_some() && (playing || self.expanded || self.progress > 0.) && !blocked;
        if show {
            self.place(hwnd);
            if !self.visible {
                self.visible = true;
                let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
            }
            let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
        } else if self.visible {
            self.visible = false;
            self.unhook_outside();
            self.expanded = false;
            self.progress = 0.;
            self.motion = None;
            self.hover = None;
            unsafe {
                let _ = KillTimer(Some(hwnd), ANIMATE);
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }
    }

    fn place(&mut self, hwnd: HWND) {
        unsafe {
            let monitor = MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY);
            let mut info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(monitor, &mut info).as_bool() {
                return;
            }
            // GetDpiForWindow follows the window. Move it first on a primary-display change.
            if MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL) != monitor {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    info.rcMonitor.left,
                    info.rcMonitor.top,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOREDRAW,
                );
            }
            let dpi = GetDpiForWindow(hwnd).max(96);
            let scale = dpi as f32 / 96.;
            let px = |dip: f32| (dip * scale).round() as i32;
            let lerp = |a, b| a + (b - a) * self.progress;
            let width = px(lerp(COMPACT.0, EXPANDED.0));
            let height = px(lerp(COMPACT.1, EXPANDED.1));
            let x = info.rcMonitor.left + (info.rcMonitor.right - info.rcMonitor.left - width) / 2;
            let y = info.rcWork.top + px(9.);
            let next = (x, y, width, height, dpi);
            if self.geometry != Some(next) {
                self.geometry = Some(next);
                crate::theme::shape(hwnd, width, height, lerp(19., 26.), dpi);
                let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, width, height, SWP_NOACTIVATE);
            }
        }
    }

    fn set_expanded(&mut self, hwnd: HWND, expanded: bool) {
        if self.expanded == expanded {
            return;
        }
        self.expanded = expanded;
        if expanded {
            self.hook_outside(hwnd);
        } else {
            self.unhook_outside();
        }
        if crate::dock::client_animations_enabled() && unsafe { SetTimer(Some(hwnd), ANIMATE, 15, None) } != 0 {
            self.motion = Some((Instant::now(), self.progress));
        } else {
            self.motion = None;
            self.progress = if expanded { 1. } else { 0. };
            self.sync_visibility(hwnd);
        }
    }

    fn animate(&mut self, hwnd: HWND) {
        let Some((start, from)) = self.motion else { return };
        let to = if self.expanded { 1. } else { 0. };
        let duration = if self.expanded { 0.28 } else { 0.22 };
        let t = (start.elapsed().as_secs_f32() / duration).min(1.);
        self.progress = from + (to - from) * (1. - (1. - t).powi(3));
        if t >= 1. {
            self.motion = None;
            let _ = unsafe { KillTimer(Some(hwnd), ANIMATE) };
        }
        self.sync_visibility(hwnd);
    }

    fn poll(&mut self, hwnd: HWND) {
        if self.last_request.elapsed() >= Duration::from_millis(840) {
            self.request(hwnd);
        }
        self.sync_visibility(hwnd);
    }

    fn hook_outside(&mut self, hwnd: HWND) {
        // While expanded, observe button-down positions and pass every click through.
        let hook = unsafe {
            GetModuleHandleW(None)
                .and_then(|module| SetWindowsHookExW(WH_MOUSE_LL, Some(outside_click), Some(module.into()), 0))
        };
        if let Ok(hook) = hook {
            EXPANDED_NOTCH.with(|target| target.set(hwnd));
            self.outside_hook = Some(hook);
        }
    }

    fn unhook_outside(&mut self) {
        if let Some(hook) = self.outside_hook.take() {
            EXPANDED_NOTCH.with(|target| target.set(HWND::default()));
            let _ = unsafe { UnhookWindowsHookEx(hook) };
        }
    }

    fn click(&mut self, hwnd: HWND, x: f32, y: f32) {
        if !self.expanded {
            self.set_expanded(hwnd, true);
            return;
        }
        if y < 42. && x > 304. {
            self.set_expanded(hwnd, false);
            return;
        }
        if self.progress < 0.95 {
            return;
        }
        let Some(track) = &self.track else { return };
        let action = if (130. ..170.).contains(&y) {
            match x {
                88. ..146. => Some(Control::Previous),
                146. ..208. => Some(Control::PlayPause),
                208. ..266. => Some(Control::Next),
                _ => None,
            }
        } else if (93. ..118.).contains(&y) && (20. ..334.).contains(&x) {
            track.media.timeline.and_then(|timeline| {
                let (min, max) = timeline.seek?;
                let fraction = f64::from((x - 24.) / 306.).clamp(0., 1.);
                let position = timeline.start + ((timeline.end - timeline.start) as f64 * fraction) as i64;
                Some(Control::Seek {
                    source: track.media.source.clone(),
                    title: track.media.title.clone(),
                    position: position.clamp(min, max),
                })
            })
        } else {
            None
        };
        if let Some(action) = action {
            self.action = Some(Control::for_track(&track.media, action));
            let _ = unsafe { PostMessageW(Some(self.owner), ACTION, WPARAM(hwnd.0 as usize), LPARAM(0)) };
        }
    }

    fn point(&self, lp: LPARAM) -> (f32, f32) {
        let scale = self.geometry.map_or(1., |(_, _, _, _, dpi)| dpi as f32 / 96.);
        ((lp.0 as i16) as f32 / scale, ((lp.0 >> 16) as i16) as f32 / scale)
    }
}

unsafe extern "system" fn outside_click(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 && matches!(wp.0 as u32, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN) {
            let hwnd = EXPANDED_NOTCH.with(Cell::get);
            let mut rect = RECT::default();
            if !hwnd.is_invalid() && GetWindowRect(hwnd, &mut rect).is_ok() {
                let point = (*(lp.0 as *const MSLLHOOKSTRUCT)).pt;
                if !(rect.left..rect.right).contains(&point.x) || !(rect.top..rect.bottom).contains(&point.y) {
                    let _ = PostMessageW(Some(hwnd), COLLAPSE, WPARAM(0), LPARAM(0));
                }
            }
        }
        CallNextHookEx(None, code, wp, lp)
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_NCCREATE => {
                let create = &*(lp.0 as *const CREATESTRUCTW);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
            WM_ERASEBKGND => return LRESULT(1),
            _ => {}
        }
        let cell = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<State>;
        if let Some(cell) = cell.as_ref()
            && let Ok(mut state) = cell.try_borrow_mut()
        {
            match msg {
                WM_TIMER if wp.0 == POLL => state.poll(hwnd),
                WM_TIMER if wp.0 == ANIMATE => state.animate(hwnd),
                COLLAPSE => state.set_expanded(hwnd, false),
                WM_LBUTTONUP => {
                    let (x, y) = state.point(lp);
                    state.click(hwnd, x, y);
                }
                WM_MOUSEMOVE => {
                    use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
                    state.hover = Some(state.point(lp));
                    let _ = TrackMouseEvent(&mut TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    });
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                windows::Win32::UI::Controls::WM_MOUSELEAVE => {
                    state.hover = None;
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                WM_PAINT => {
                    let mut paint = PAINTSTRUCT::default();
                    let _ = BeginPaint(hwnd, &mut paint);
                    let State {
                        renderer,
                        palette,
                        track,
                        progress,
                        geometry,
                        hover,
                        ..
                    } = &mut *state;
                    if let Some(track) = track {
                        let dpi = geometry.map_or(96, |(_, _, _, _, dpi)| dpi);
                        let _ = renderer.draw_notch(hwnd, dpi, palette, track, *progress, *hover);
                    }
                    let _ = EndPaint(hwnd, &paint);
                }
                _ => return DefWindowProcW(hwnd, msg, wp, lp),
            }
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}
