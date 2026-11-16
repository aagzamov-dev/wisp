//! Owned synchronous Start app popup. The modal pump holds no Launcher borrow;
//! renderer/material calls likewise hold no window-state borrow.
use crate::{config::Backdrop, dock::render::Renderer, theme::SurfaceMaterial};
use std::{
    cell::Cell,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{HiDpi::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
    core::{Result, w},
};
#[path = "app_menu/accessibility.rs"]
mod accessibility;

const TIMER: usize = 1;
const ACCESS_FOCUS: u32 = WM_APP + 71;
const ACCESS_INVOKE: u32 = WM_APP + 72;
const CLASS: windows::core::PCWSTR = w!("Wisp.StartAppMenu");
static NONCE: AtomicUsize = AtomicUsize::new(1);
pub(crate) const WIDTH: f32 = 304.;
pub(crate) const PAD: f32 = 8.;
pub(crate) const RADIUS: f32 = 14.;

#[derive(Clone, Debug)]
pub(crate) struct Row {
    pub label: &'static str,
    pub reason: Option<&'static str>,
    pub glyph: &'static str,
}
impl Row {
    pub fn enabled(&self) -> bool {
        self.reason.is_none()
    }
    pub fn height(&self) -> f32 {
        if self.reason.is_some() { 48. } else { 34. }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Appearance {
    pub dark: bool,
    pub backdrop: Backdrop,
    pub color: String,
    pub material_options: crate::theme::MaterialOptions,
    pub remote: bool,
    pub opening: bool,
    pub closing: bool,
}

/// Input identities are stable row indices in an immutable app/action snapshot.
pub(crate) struct Model {
    pub rows: Vec<Row>,
    pub focus: Option<usize>,
    pub scroll: f32,
    pub view: f32,
    pressed: Option<(usize, u32)>,
    enter: Option<usize>,
    pub closing: bool,
    pub picked: Option<usize>,
}
impl Model {
    fn new(rows: Vec<Row>, view: f32) -> Self {
        let focus = rows.iter().position(Row::enabled);
        Self {
            rows,
            focus,
            scroll: 0.,
            view,
            pressed: None,
            enter: None,
            closing: false,
            picked: None,
        }
    }
    pub fn top(&self, index: usize) -> f32 {
        PAD + self.rows[..index].iter().map(Row::height).sum::<f32>() - self.scroll
    }
    pub fn max_scroll(&self) -> f32 {
        (2. * PAD + self.rows.iter().map(Row::height).sum::<f32>() - self.view).max(0.)
    }
    fn hit(&self, x: f32, y: f32, width: f32) -> Option<usize> {
        if x < PAD || x >= width - PAD || y < PAD || y >= self.view - PAD {
            return None;
        }
        self.rows
            .iter()
            .enumerate()
            .position(|(i, row)| y >= self.top(i) && y < self.top(i) + row.height())
    }
    fn focus(&mut self, index: usize) {
        if !self.rows.get(index).is_some_and(Row::enabled) || self.closing {
            return;
        }
        self.focus = Some(index);
        let top = self.top(index);
        let bottom = top + self.rows[index].height();
        if top < PAD {
            self.scroll += top - PAD;
        }
        if bottom > self.view - PAD {
            self.scroll += bottom - (self.view - PAD);
        }
        self.scroll = self.scroll.clamp(0., self.max_scroll());
    }
    fn navigate(&mut self, step: i32, edge: bool) {
        self.enter = None;
        self.pressed = None;
        let count = self.rows.len();
        if count == 0 {
            return;
        }
        let start = if edge {
            if step > 0 { count - 1 } else { 0 }
        } else {
            self.focus.unwrap_or(if step > 0 { count - 1 } else { 0 })
        };
        for offset in 1..=count {
            let i = (start as i32 + step * offset as i32).rem_euclid(count as i32) as usize;
            if self.rows[i].enabled() {
                self.focus(i);
                break;
            }
        }
    }
    fn press(&mut self, index: Option<usize>, button: u32) {
        if !self.closing {
            self.pressed = index.filter(|i| self.rows[*i].enabled()).map(|i| (i, button));
            self.enter = None;
        }
    }
    fn release(&mut self, index: Option<usize>, button: u32) {
        let down = self.pressed.take();
        if down.is_some() && down == index.map(|i| (i, button)) {
            self.select(index);
        }
    }
    fn select(&mut self, index: Option<usize>) {
        if self.closing {
            return;
        }
        self.picked = index.filter(|i| self.rows.get(*i).is_some_and(Row::enabled));
        self.closing = true;
        self.pressed = None;
        self.enter = None;
    }
}

fn bounds(work: RECT, anchor: POINT, dpi: u32, height: f32) -> RECT {
    let scale = dpi.max(1) as f32 / 96.;
    let width = ((WIDTH * scale).round() as i32).min((work.right - work.left).max(1));
    let height = ((height * scale).round() as i32).min((work.bottom - work.top).max(1));
    let left = anchor.x.clamp(work.left, work.right - width);
    let top = anchor.y.saturating_sub(height).clamp(work.top, work.bottom - height);
    RECT {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}

fn event_point(point: POINT, area: RECT, scale: f32) -> (f32, f32) {
    (
        (point.x - area.left) as f32 / scale,
        (point.y - area.top) as f32 / scale,
    )
}

#[derive(Debug)]
struct Fade(crate::animation::Motion);
impl std::ops::Deref for Fade {
    type Target = crate::animation::Motion;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl Fade {
    fn new(appearance: &Appearance) -> Self {
        let mut motion = crate::animation::Motion::default();
        motion.retarget_duration(
            true,
            if appearance.opening {
                if appearance.remote { 120 } else { 180 }
            } else {
                0
            },
        );
        Self(motion)
    }
    fn close(&mut self, appearance: &Appearance) {
        self.0.retarget_duration(
            false,
            if appearance.closing {
                if appearance.remote { 90 } else { 140 }
            } else {
                0
            },
        );
    }
    fn tick(&mut self, seconds: f32) {
        self.0.tick(seconds);
    }
    fn alpha(&self) -> u8 {
        (self.value * 255.).round().clamp(0., 255.) as u8
    }
}

struct Bridge {
    cancelled: Cell<bool>,
    capture: Cell<bool>,
    access: Arc<accessibility::Snapshot>,
}
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let create = &*(lp.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Bridge;
        if !ptr.is_null() {
            // Clone before any COM call: WM_GETOBJECT can reenter destruction.
            let bridge = &*ptr;
            match msg {
                WM_GETOBJECT if lp.0 as i32 == windows::Win32::UI::Accessibility::UiaRootObjectId => {
                    let access = bridge.access.clone();
                    return accessibility::get_object(hwnd, wp, lp, access);
                }
                WM_CAPTURECHANGED if bridge.capture.get() => bridge.cancelled.set(true),
                WM_ACTIVATE if wp.0 & 0xffff == WA_INACTIVE as usize => bridge.cancelled.set(true),
                WM_CANCELMODE | WM_CLOSE => bridge.cancelled.set(true),
                WM_NCDESTROY => {
                    bridge.cancelled.set(true);
                    bridge.access.deactivate();
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                _ => {}
            }
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

struct Window {
    hwnd: HWND,
    bridge: Box<Bridge>,
}
impl Window {
    fn owns_hwnd(&self) -> bool {
        // The owner may destroy us in a nested callback and Windows may reuse
        // the numeric handle before this stack frame unwinds.
        unsafe { GetWindowLongPtrW(self.hwnd, GWLP_USERDATA) == (&*self.bridge as *const Bridge) as isize }
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        self.bridge.access.deactivate();
        self.bridge.capture.set(false);
        unsafe {
            if !self.owns_hwnd() {
                return;
            }
            let _ = KillTimer(Some(self.hwnd), TIMER);
            if GetCapture() == self.hwnd {
                let _ = ReleaseCapture();
            }
            if self.owns_hwnd() {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

#[derive(Debug, PartialEq)]
enum Read {
    Message,
    Quit(i32),
    Failed,
}
fn read_result(received: i32, code: usize) -> Read {
    match received {
        0 => Read::Quit(code as i32),
        n if n < 0 => Read::Failed,
        _ => Read::Message,
    }
}
fn consumes_input(message: u32) -> bool {
    matches!(
        message,
        WM_KEYDOWN
            | WM_SYSKEYDOWN
            | WM_KEYUP
            | WM_SYSKEYUP
            | WM_CHAR
            | WM_SYSCHAR
            | WM_DEADCHAR
            | WM_SYSDEADCHAR
            | WM_IME_CHAR
            | WM_UNICHAR
    )
}

#[derive(Debug, PartialEq)]
enum FrameUpdate {
    None,
    Cached,
    Redraw,
}
fn frame_update(content_changed: bool, material_fallback: bool, opacity_tick: bool) -> FrameUpdate {
    if content_changed || material_fallback {
        FrameUpdate::Redraw
    } else if opacity_tick {
        FrameUpdate::Cached
    } else {
        FrameUpdate::None
    }
}

/// `valid` revalidates owner/session/index/config after every dispatched message.
/// `dispatch` is the application's shared dispatch path (including frame pacing).
pub(crate) fn pick(
    owner: HWND,
    anchor: POINT,
    rows: Vec<Row>,
    appearance: Appearance,
    valid: impl Fn() -> bool,
    dispatch: impl Fn(&MSG),
) -> Option<usize> {
    match show(owner, anchor, rows, appearance, valid, dispatch) {
        Ok(picked) => picked,
        Err(error) => {
            crate::logging::error("start.app.menu.failed", format_args!("error={error}"));
            None
        }
    }
}

fn show(
    owner: HWND,
    anchor: POINT,
    rows: Vec<Row>,
    appearance: Appearance,
    valid: impl Fn() -> bool,
    dispatch: impl Fn(&MSG),
) -> Result<Option<usize>> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            return Err(windows::core::Error::from_thread());
        }
        let monitor = MonitorFromPoint(anchor, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return Err(windows::core::Error::from_thread());
        }
        let mut dx = 96;
        let mut dy = 96;
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let area = bounds(
            info.rcWork,
            anchor,
            dx,
            PAD * 2. + rows.iter().map(Row::height).sum::<f32>(),
        );
        let scale = dx.max(1) as f32 / 96.;
        let mut model = Model::new(rows, (area.bottom - area.top) as f32 / scale);
        let access = Arc::new(accessibility::Snapshot::new(
            NONCE.fetch_add(1, Ordering::Relaxed),
            &model.rows,
        ));
        let bridge = Box::new(Bridge {
            cancelled: Cell::new(false),
            capture: Cell::new(false),
            access,
        });
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            CLASS,
            w!("App actions"),
            WS_POPUP,
            area.left,
            area.top,
            area.right - area.left,
            area.bottom - area.top,
            Some(owner),
            None,
            Some(instance.into()),
            Some((&*bridge as *const Bridge).cast()),
        )?;
        let window = Window { hwnd, bridge };
        window.bridge.access.attach(hwnd);
        if !valid() || window.bridge.cancelled.get() {
            return Ok(None);
        }
        let mut renderer = Renderer::new()?;
        if !valid() || window.bridge.cancelled.get() {
            return Ok(None);
        }
        // The material host is a sibling owned by Start, never owned by the
        // foreground popup (which would force the host above it).
        let mut material = SurfaceMaterial::prepare(
            Some(owner),
            hwnd,
            appearance.dark,
            appearance.backdrop,
            &appearance.color,
            appearance.material_options,
        );
        if !valid() || window.bridge.cancelled.get() {
            return Ok(None);
        }
        let mut fade = Fade::new(&appearance);
        renderer.draw_app_menu(hwnd, dx, &material.palette, &model, fade.alpha())?;
        window.bridge.access.update(&model, area, scale);
        if !valid() || window.bridge.cancelled.get() {
            return Ok(None);
        }
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        if !valid() || window.bridge.cancelled.get() {
            return Ok(None);
        }
        if !material.present(area, dx, RADIUS, fade.value) {
            renderer.draw_app_menu(hwnd, dx, &material.palette, &model, fade.alpha())?;
        }
        if !valid() || window.bridge.cancelled.get() {
            return Ok(None);
        }
        let _ = SetForegroundWindow(hwnd);
        if !valid() || window.bridge.cancelled.get() {
            return Ok(None);
        }
        let _ = SetFocus(Some(hwnd));
        if !valid() || window.bridge.cancelled.get() {
            return Ok(None);
        }
        SetCapture(hwnd);
        window.bridge.capture.set(true);
        if GetCapture() != hwnd || !valid() || window.bridge.cancelled.get() {
            return Ok(None);
        }
        accessibility::announce(&window.bridge.access, model.focus, true);
        let mut timer = false;
        if fade.active() {
            if SetTimer(Some(hwnd), TIMER, crate::animation::interval(appearance.remote), None) == 0 {
                return Err(windows::core::Error::from_thread());
            }
            timer = true;
        }
        let mut last_tick = Instant::now();
        let mut quit = None;
        let mut redraw = false;
        let mut restore_focus = true;
        loop {
            if !valid() || window.bridge.cancelled.get() || !window.owns_hwnd() {
                model.picked = None;
                break;
            }
            if model.closing && !fade.active() {
                break;
            }
            let mut msg = MSG::default();
            let received = GetMessageW(&mut msg, None, 0, 0).0;
            match read_result(received, msg.wParam.0) {
                Read::Message => {}
                Read::Quit(code) => {
                    quit = Some(code);
                    model.picked = None;
                    break;
                }
                Read::Failed => {
                    crate::logging::error(
                        "start.app.menu.message_loop",
                        format_args!("error={}", windows::core::Error::from_thread()),
                    );
                    model.picked = None;
                    break;
                }
            }
            let was_closing = model.closing;
            let ours = msg.hwnd == hwnd;
            // If Windows declined activation, keyboard messages may still go
            // to Start. The same modal menu consumes them without editing the
            // query or launching the row underneath it.
            let input = ours || msg.hwnd == owner && consumes_input(msg.message);
            let point = || {
                // MSG.pt belongs to this queued event. Sampling the current
                // cursor here could retarget a delayed down/up pair after the
                // pointer has already moved to a different action.
                Some(event_point(msg.pt, area, scale))
            };
            if input && !model.closing {
                match msg.message {
                    WM_MOUSEMOVE => {
                        if let Some((x, y)) = point() {
                            let hover = model.hit(x, y, (area.right - area.left) as f32 / scale);
                            if let Some(i) = hover.filter(|i| model.rows[*i].enabled())
                                && model.focus != Some(i)
                            {
                                model.focus = Some(i);
                                redraw = true;
                            }
                        }
                    }
                    WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
                        if let Some((x, y)) = point() {
                            let width = (area.right - area.left) as f32 / scale;
                            if x < 0. || x >= width || y < 0. || y >= model.view {
                                restore_focus = false;
                                model.select(None);
                            } else {
                                model.press(model.hit(x, y, width), msg.message);
                            }
                        }
                    }
                    WM_LBUTTONUP | WM_RBUTTONUP => model.release(
                        point().and_then(|(x, y)| model.hit(x, y, (area.right - area.left) as f32 / scale)),
                        if msg.message == WM_LBUTTONUP {
                            WM_LBUTTONDOWN
                        } else {
                            WM_RBUTTONDOWN
                        },
                    ),
                    WM_MBUTTONDOWN | WM_XBUTTONDOWN => {
                        restore_focus = false;
                        model.select(None)
                    }
                    WM_MOUSEWHEEL => {
                        model.pressed = None;
                        model.enter = None;
                        model.scroll = (model.scroll - ((msg.wParam.0 >> 16) as i16 as f32 / 120.) * 68.)
                            .clamp(0., model.max_scroll());
                        redraw = true;
                    }
                    WM_KEYDOWN | WM_SYSKEYDOWN => match VIRTUAL_KEY(msg.wParam.0 as u16) {
                        VK_ESCAPE | VK_MENU => model.select(None),
                        VK_UP => {
                            model.navigate(-1, false);
                            redraw = true;
                        }
                        VK_DOWN => {
                            model.navigate(1, false);
                            redraw = true;
                        }
                        VK_HOME => {
                            model.navigate(1, true);
                            redraw = true;
                        }
                        VK_END => {
                            model.navigate(-1, true);
                            redraw = true;
                        }
                        VK_TAB => {
                            model.navigate(if GetKeyState(VK_SHIFT.0 as i32) < 0 { -1 } else { 1 }, false);
                            redraw = true;
                        }
                        VK_RETURN if msg.lParam.0 & (1 << 30) == 0 => model.enter = model.focus,
                        _ => {}
                    },
                    WM_KEYUP if VIRTUAL_KEY(msg.wParam.0 as u16) == VK_RETURN => {
                        let pressed = model.enter.take();
                        if pressed.is_some() && pressed == model.focus {
                            model.select(pressed);
                        }
                    }
                    ACCESS_FOCUS | ACCESS_INVOKE if msg.wParam.0 == window.bridge.access.nonce => {
                        let i = msg.lParam.0 as usize;
                        if msg.message == ACCESS_FOCUS {
                            model.focus(i);
                            redraw = true;
                        } else {
                            model.select(Some(i));
                        }
                    }
                    _ => {}
                }
            }
            if matches!(
                msg.message,
                WM_DPICHANGED | WM_DISPLAYCHANGE | WM_SETTINGCHANGE | WM_THEMECHANGED | WM_SYSCOLORCHANGE
            ) {
                // Dispatch the owner event first, then cancel this immutable
                // popup; Start must still receive its theme/DPI update.
                dispatch(&msg);
                model.picked = None;
                break;
            }
            // Dispatch unrelated lifecycle/index/config events with the exact
            // same frame-pending bookkeeping as the outer application pump.
            if !(input && consumes_input(msg.message) || ours && matches!(msg.message, ACCESS_FOCUS | ACCESS_INVOKE)) {
                dispatch(&msg);
            }
            if !valid() || window.bridge.cancelled.get() {
                model.picked = None;
                break;
            }
            if !was_closing && model.closing {
                window.bridge.capture.set(false);
                if GetCapture() == hwnd {
                    let _ = ReleaseCapture();
                }
                fade.close(&appearance);
                if fade.active() && !timer {
                    if SetTimer(Some(hwnd), TIMER, crate::animation::interval(appearance.remote), None) == 0 {
                        model.picked = None;
                        break;
                    }
                    timer = true;
                }
                last_tick = Instant::now();
            }
            let opacity_tick = ours && msg.message == WM_TIMER && msg.wParam.0 == TIMER;
            let mut material_fallback = false;
            if opacity_tick {
                fade.tick(last_tick.elapsed().as_secs_f32());
                last_tick = Instant::now();
                if !fade.active() && timer {
                    let _ = KillTimer(Some(hwnd), TIMER);
                    timer = false;
                }
                if !material.present(area, dx, RADIUS, fade.value) {
                    material_fallback = true;
                }
                if !valid() || window.bridge.cancelled.get() {
                    model.picked = None;
                    break;
                }
            }
            match frame_update(redraw, material_fallback, opacity_tick) {
                FrameUpdate::Cached => renderer.present_cached_popup(hwnd, dx, fade.alpha())?,
                FrameUpdate::Redraw => {
                    let old_focus = window.bridge.access.focused_row();
                    window.bridge.access.update(&model, area, scale);
                    renderer.draw_app_menu(hwnd, dx, &material.palette, &model, fade.alpha())?;
                    if old_focus != model.focus {
                        accessibility::announce(&window.bridge.access, model.focus, false);
                    }
                    redraw = false;
                }
                FrameUpdate::None => {}
            }
        }
        material.hide();
        let restore = model.closing
            && restore_focus
            && !window.bridge.cancelled.get()
            && quit.is_none()
            && GetForegroundWindow() == hwnd
            && valid();
        // Explicitly destroy material target before the foreground and before
        // returning to Launcher. The outer pump alone owns runtime shutdown.
        drop(material);
        let picked = model.picked;
        drop(window);
        if restore && valid() {
            let _ = SetForegroundWindow(owner);
            if valid() {
                let _ = SetFocus(Some(owner));
            }
        }
        if let Some(code) = quit {
            PostQuitMessage(code);
        }
        Ok(picked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rows() -> Vec<Row> {
        (0..8)
            .map(|i| Row {
                label: "Action",
                reason: if i == 1 { Some("Unavailable") } else { None },
                glyph: "",
            })
            .collect()
    }
    #[test]
    fn selection_requires_same_press_release_and_is_single() {
        let mut m = Model::new(rows(), 300.);
        m.press(Some(0), WM_LBUTTONDOWN);
        m.release(Some(2), WM_LBUTTONDOWN);
        assert!(!m.closing);
        m.press(Some(1), WM_LBUTTONDOWN);
        m.release(Some(1), WM_LBUTTONDOWN);
        assert!(!m.closing);
        m.press(Some(2), WM_LBUTTONDOWN);
        m.release(Some(2), WM_RBUTTONDOWN);
        assert!(!m.closing);
        m.press(Some(2), WM_LBUTTONDOWN);
        m.release(Some(2), WM_LBUTTONDOWN);
        assert_eq!(m.picked, Some(2));
        m.select(Some(3));
        assert_eq!(m.picked, Some(2));
    }
    #[test]
    fn cancellation_cannot_select() {
        let mut m = Model::new(rows(), 300.);
        m.press(Some(0), WM_LBUTTONDOWN);
        m.select(None);
        m.release(Some(0), WM_LBUTTONDOWN);
        assert_eq!(m.picked, None);
    }
    #[test]
    fn navigation_skips_disabled_and_scrolls_focus_into_view() {
        let mut m = Model::new(rows(), 120.);
        m.navigate(1, false);
        assert_eq!(m.focus, Some(2));
        m.navigate(-1, true);
        assert_eq!(m.focus, Some(7));
        assert!(m.top(7) + m.rows[7].height() <= m.view - PAD + 0.001);
        m.navigate(1, true);
        assert_eq!(m.focus, Some(0));
        assert_eq!(m.scroll, 0.);
    }
    #[test]
    fn bounds_clamp_negative_monitors_and_scaled_short_workareas() {
        for dpi in [96, 144, 192] {
            let work = RECT {
                left: -1600,
                top: -200,
                right: -800,
                bottom: 0,
            };
            let b = bounds(work, POINT { x: -790, y: 20 }, dpi, 400.);
            assert!(b.left >= work.left && b.right <= work.right && b.top >= work.top && b.bottom <= work.bottom);
        }
    }
    #[test]
    fn fade_is_monotone_bounded_and_stops_with_remote_caps() {
        let a = Appearance {
            dark: true,
            material_options: Default::default(),
            backdrop: Backdrop::Glass,
            color: String::new(),
            remote: true,
            opening: true,
            closing: true,
        };
        let mut f = Fade::new(&a);
        let mut previous = 0.;
        for _ in 0..8 {
            f.tick(0.015);
            assert!(f.value >= previous && f.value <= 1.);
            previous = f.value;
        }
        assert!(!f.active());
        assert_eq!(f.alpha(), 255);
        f.close(&a);
        for _ in 0..7 {
            f.tick(0.015);
        }
        assert!(!f.active());
        assert_eq!(f.alpha(), 0);
    }
    #[test]
    fn quit_keeps_signed_exit_code_and_failure_is_distinct() {
        assert_eq!(read_result(0, (-9_i32) as usize), Read::Quit(-9));
        assert_eq!(read_result(-1, 99), Read::Failed);
        assert_eq!(read_result(1, 0), Read::Message);
    }
    #[test]
    fn keyboard_navigation_cancels_pointer_press() {
        let mut m = Model::new(rows(), 300.);
        m.press(Some(0), WM_LBUTTONDOWN);
        m.navigate(1, false);
        m.release(Some(0), WM_LBUTTONDOWN);
        assert!(!m.closing);
    }
    #[test]
    fn reduced_motion_settles_without_timer() {
        let a = Appearance {
            dark: false,
            material_options: Default::default(),
            backdrop: Backdrop::Solid,
            color: String::new(),
            remote: false,
            opening: false,
            closing: false,
        };
        let mut f = Fade::new(&a);
        assert!(!f.active());
        assert_eq!(f.alpha(), 255);
        f.close(&a);
        assert!(!f.active());
        assert_eq!(f.alpha(), 0);
    }
    #[test]
    fn opacity_ticks_only_present_cached_pixels_and_fallback_forces_redraw() {
        assert_eq!(frame_update(false, false, true), FrameUpdate::Cached);
        assert_eq!(frame_update(true, false, true), FrameUpdate::Redraw);
        assert_eq!(frame_update(false, true, true), FrameUpdate::Redraw);
        assert_eq!(frame_update(false, false, false), FrameUpdate::None);
    }
    #[test]
    fn queued_pointer_events_keep_their_original_scaled_coordinates() {
        let area = RECT {
            left: -900,
            top: 100,
            right: -596,
            bottom: 400,
        };
        let mut m = Model::new(rows(), 300.);
        let down = event_point(POINT { x: -870, y: 120 }, area, 1.);
        let up = event_point(POINT { x: -870, y: 215 }, area, 1.);
        m.press(m.hit(down.0, down.1, WIDTH), WM_LBUTTONDOWN);
        m.release(m.hit(up.0, up.1, WIDTH), WM_LBUTTONDOWN);
        assert!(!m.closing);
        assert_eq!(event_point(POINT { x: -870, y: 120 }, area, 2.), (15., 10.));
    }
    #[test]
    fn modal_input_routing_keeps_queued_text_out_of_the_start_query() {
        for message in [
            WM_KEYDOWN,
            WM_KEYUP,
            WM_SYSKEYDOWN,
            WM_SYSKEYUP,
            WM_CHAR,
            WM_SYSCHAR,
            WM_DEADCHAR,
            WM_SYSDEADCHAR,
            WM_IME_CHAR,
            WM_UNICHAR,
        ] {
            assert!(consumes_input(message));
        }
        for message in [WM_TIMER, WM_PAINT, WM_APP + 8, WM_SETTINGCHANGE, WM_DESTROY] {
            assert!(!consumes_input(message));
        }
    }
}
