//! Reserves the selected monitors' top bands without waiting for Explorer on
//! the UI thread. Every reservation belongs to a hidden window on this worker.

use std::{
    cell::Cell,
    collections::HashMap,
    sync::{Arc, Mutex, mpsc},
    thread::JoinHandle,
};

use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
            Shell::{ABE_TOP, ABM_NEW, ABM_QUERYPOS, ABM_REMOVE, ABM_SETPOS, ABN_POSCHANGED, APPBARDATA},
            WindowsAndMessaging::*,
        },
    },
    core::{BOOL, Error, HRESULT, Result, w},
};

use super::space::send;

const UPDATE: u32 = WM_APP + 50;
const CHECK: u32 = WM_APP + 51;
const APPBAR: u32 = WM_APP + 52;
const RESTART: u32 = WM_APP + 53;
const POSITION_CHANGED: u32 = WM_APP + 54;
const TIMER: usize = 1;
const RETRY_CHECKS: u8 = 10;

thread_local! {
    static CONTROLLER: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
    static TASKBAR_CREATED: Cell<u32> = const { Cell::new(0) };
}

type Targets = Arc<Mutex<Vec<(String, f32)>>>;
/// Published by the worker after Explorer negotiates each reservation.
pub type Strips = Arc<Mutex<HashMap<String, RECT>>>;

/// A UI-thread caller must never wait while Explorer is working.
pub fn strip(strips: &Strips, id: &str) -> Option<RECT> {
    strips.try_lock().ok()?.get(&id.to_ascii_lowercase()).copied()
}

pub struct TopbarSpace {
    hwnd: HWND,
    targets: Targets,
    strips: Strips,
    thread: Option<JoinHandle<()>>,
}

impl TopbarSpace {
    pub fn new(targets: Vec<(String, f32)>) -> Result<Self> {
        let targets = Arc::new(Mutex::new(normalize(targets)));
        let worker_targets = Arc::clone(&targets);
        let strips = Arc::new(Mutex::new(HashMap::new()));
        let worker_strips = Arc::clone(&strips);
        let (ready, started) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("top bar space".into())
            .spawn(move || run(worker_targets, worker_strips, ready))
            .map_err(|error| Error::new(E_FAIL, error.to_string()))?;
        match started.recv() {
            Ok(Ok(hwnd)) => Ok(Self {
                hwnd: HWND(hwnd as _),
                targets,
                strips,
                thread: Some(thread),
            }),
            Ok(Err(code)) => {
                let _ = thread.join();
                Err(Error::from_hresult(code))
            }
            Err(_) => {
                let _ = thread.join();
                Err(Error::new(E_FAIL, "The top bar space thread exited"))
            }
        }
    }

    pub fn strips(&self) -> Strips {
        Arc::clone(&self.strips)
    }

    /// Coalesces rapid settings edits; no shell calls or cross-thread pointer
    /// ownership are needed on the UI thread.
    pub fn set_targets(&self, targets: Vec<(String, f32)>) {
        let targets = normalize(targets);
        let mut current = self.targets.lock().unwrap_or_else(|error| error.into_inner());
        if *current == targets {
            return;
        }
        *current = targets;
        drop(current);
        let _ = unsafe { PostMessageW(Some(self.hwnd), UPDATE, WPARAM(0), LPARAM(0)) };
    }
}

impl Drop for TopbarSpace {
    fn drop(&mut self) {
        if self.thread.take().is_some() {
            // The worker removes every appbar before destroying its windows.
            // Like DockSpace, do not wait for Explorer from a settings callback.
            let _ = unsafe { PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        }
    }
}

fn normalize(targets: Vec<(String, f32)>) -> Vec<(String, f32)> {
    let mut result: Vec<(String, f32)> = Vec::new();
    for (id, height) in targets {
        if id.is_empty() || !height.is_finite() {
            continue;
        }
        let height = height.clamp(24., 48.);
        if let Some((_, previous)) = result
            .iter_mut()
            .find(|(previous, _)| previous.eq_ignore_ascii_case(&id))
        {
            *previous = previous.max(height);
        } else {
            result.push((id, height));
        }
    }
    result
}

struct Display {
    id: String,
    monitor: RECT,
    work: RECT,
    dpi: u32,
}

fn displays() -> Vec<Display> {
    unsafe extern "system" fn collect(handle: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
        unsafe {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            if GetMonitorInfoW(handle, &mut info.monitorInfo).as_bool() {
                let end = info
                    .szDevice
                    .iter()
                    .position(|value| *value == 0)
                    .unwrap_or(info.szDevice.len());
                let (mut x, mut y) = (96, 96);
                let _ = GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut x, &mut y);
                (*(lp.0 as *mut Vec<Display>)).push(Display {
                    id: String::from_utf16_lossy(&info.szDevice[..end]),
                    monitor: info.monitorInfo.rcMonitor,
                    work: info.monitorInfo.rcWork,
                    dpi: y.clamp(96, 768),
                });
            }
            BOOL(1)
        }
    }
    let mut result = Vec::new();
    let _ = unsafe { EnumDisplayMonitors(None, None, Some(collect), LPARAM(&raw mut result as isize)) };
    result
}

struct Reservation {
    hwnd: HWND,
    registered: bool,
    strip: RECT,
    placed: Option<(RECT, i32)>,
    retry: u8,
}

impl Reservation {
    fn new() -> Result<Self> {
        Ok(Self {
            hwnd: create()?,
            registered: false,
            strip: RECT::default(),
            placed: None,
            retry: 0,
        })
    }

    fn data(&self) -> APPBARDATA {
        APPBARDATA {
            cbSize: size_of::<APPBARDATA>() as u32,
            hWnd: self.hwnd,
            uEdge: ABE_TOP,
            ..Default::default()
        }
    }

    fn check(&mut self, display: &Display, height: f32, restart: bool, position_changed: bool) {
        self.check_with(display, height, restart, position_changed, send);
    }

    fn check_with(
        &mut self,
        display: &Display,
        height: f32,
        restart: bool,
        position_changed: bool,
        mut shell: impl FnMut(u32, &mut APPBARDATA) -> usize,
    ) {
        if restart {
            self.registered = false;
            self.placed = None;
            self.retry = 0;
        }
        let height = (height * display.dpi as f32 / 96.).round() as i32;
        let changed = self.placed != Some((display.monitor, height));
        let kept = self.registered && self.placed.is_some() && display.work.top >= self.strip.bottom;
        if kept && !changed && !position_changed {
            return;
        }
        if !self.registered {
            if self.retry > 0 {
                self.retry -= 1;
                return;
            }
            let mut data = self.data();
            data.uCallbackMessage = APPBAR;
            self.registered = shell(ABM_NEW, &mut data) != 0;
            if !self.registered {
                self.retry = RETRY_CHECKS;
                return;
            }
        }
        let mut data = self.data();
        data.rc = display.monitor;
        data.rc.bottom = data.rc.top + height;
        shell(ABM_QUERYPOS, &mut data);
        data.rc.bottom = data.rc.top + height;
        // A neighboring appbar can disappear without moving our old work-area
        // edge. Always query on ABN_POSCHANGED, then commit only a new rectangle.
        // The notification caused by our own SETPOS therefore cannot loop.
        if changed || data.rc != self.strip || (!kept && !position_changed) {
            shell(ABM_SETPOS, &mut data);
        }
        self.strip = data.rc;
        self.placed = Some((display.monitor, height));
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.registered {
            send(ABM_REMOVE, &mut self.data());
        }
        let _ = unsafe { DestroyWindow(self.hwnd) };
    }
}

fn synchronize(
    reservations: &mut HashMap<String, Reservation>,
    targets: &Targets,
    strips: &Strips,
    restart: bool,
    position_changed: bool,
) {
    let targets = targets.lock().unwrap_or_else(|error| error.into_inner()).clone();
    let displays = displays();
    reservations.retain(|id, _| {
        targets.iter().any(|(target, _)| target.eq_ignore_ascii_case(id))
            && displays.iter().any(|display| display.id.eq_ignore_ascii_case(id))
    });
    for (id, height) in targets {
        let Some(display) = displays.iter().find(|display| display.id.eq_ignore_ascii_case(&id)) else {
            continue;
        };
        let reservation = match reservations.entry(id.to_ascii_lowercase()) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let Ok(reservation) = Reservation::new() else { continue };
                entry.insert(reservation)
            }
        };
        reservation.check(display, height, restart, position_changed);
    }
    let next = reservations
        .iter()
        .filter(|(_, reservation)| reservation.registered && reservation.placed.is_some())
        .map(|(id, reservation)| (id.clone(), reservation.strip))
        .collect();
    *strips.lock().unwrap_or_else(|error| error.into_inner()) = next;
}

fn create() -> Result<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("WispTopbarSpace"),
            w!(""),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
}

fn run(targets: Targets, strips: Strips, ready: mpsc::Sender<std::result::Result<usize, HRESULT>>) {
    let initialize = || -> Result<HWND> {
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = WNDCLASSW {
                hInstance: instance.into(),
                lpfnWndProc: Some(proc),
                lpszClassName: w!("WispTopbarSpace"),
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
                return Err(Error::from_thread());
            }
            TASKBAR_CREATED.set(RegisterWindowMessageW(w!("TaskbarCreated")));
            create()
        }
    };
    let hwnd = match initialize() {
        Ok(hwnd) => hwnd,
        Err(error) => {
            let _ = ready.send(Err(error.code()));
            return;
        }
    };
    CONTROLLER.set(hwnd);
    if ready.send(Ok(hwnd.0 as usize)).is_err() {
        let _ = unsafe { DestroyWindow(hwnd) };
        CONTROLLER.set(HWND::default());
        return;
    }
    let mut reservations = HashMap::new();
    synchronize(&mut reservations, &targets, &strips, false, false);
    unsafe {
        SetTimer(Some(hwnd), TIMER, 1000, None);
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).0 > 0 {
            if message.hwnd == hwnd {
                match message.message {
                    WM_CLOSE => break,
                    UPDATE | CHECK | RESTART | POSITION_CHANGED | WM_TIMER => {
                        synchronize(
                            &mut reservations,
                            &targets,
                            &strips,
                            message.message == RESTART,
                            message.message == POSITION_CHANGED,
                        );
                        continue;
                    }
                    _ => {}
                }
            }
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        let _ = KillTimer(Some(hwnd), TIMER);
        // Destroy appbar windows on their owning thread, before the controller.
        drop(reservations);
        strips.lock().unwrap_or_else(|error| error.into_inner()).clear();
        let _ = DestroyWindow(hwnd);
    }
    CONTROLLER.set(HWND::default());
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let post = |message| {
        let controller = CONTROLLER.get();
        if !controller.is_invalid() {
            let _ = unsafe { PostMessageW(Some(controller), message, WPARAM(0), LPARAM(0)) };
        }
    };
    match msg {
        APPBAR if wp.0 as u32 == ABN_POSCHANGED => {
            post(POSITION_CHANGED);
            LRESULT(0)
        }
        WM_DISPLAYCHANGE | WM_DPICHANGED | WM_SETTINGCHANGE => {
            // Explorer may be synchronously broadcasting while recalculating
            // work areas. Acknowledge it before our worker asks Explorer again.
            let _ = unsafe { ReplyMessage(LRESULT(0)) };
            post(UPDATE);
            LRESULT(0)
        }
        _ if msg != 0 && msg == TASKBAR_CREATED.get() => {
            // This broadcast reaches all reservation windows. Register once,
            // using only the controller's copy of the notification.
            if hwnd == CONTROLLER.get() {
                post(RESTART);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_ignore_invalid_heights_and_merge_monitor_aliases() {
        let targets = normalize(vec![
            ("display1".into(), 12.),
            ("DISPLAY1".into(), 80.),
            ("display2".into(), f32::NAN),
            ("display3".into(), f32::INFINITY),
            ("".into(), 32.),
        ]);
        assert_eq!(targets, vec![("display1".into(), 48.)]);
    }

    #[test]
    fn ui_reads_negotiated_positions_without_waiting_on_the_worker() {
        let rect = RECT {
            left: -1920,
            top: 28,
            right: 0,
            bottom: 68,
        };
        let strips = Arc::new(Mutex::new(HashMap::from([("display2".into(), rect)])));
        assert_eq!(strip(&strips, "DISPLAY2"), Some(rect));
        assert_eq!(strip(&strips, "display1"), None);
        let lock = strips.lock().unwrap();
        assert_eq!(strip(&strips, "display2"), None);
        drop(lock);
        assert_eq!(strip(&strips, "display2"), Some(rect));
    }

    #[test]
    fn removing_a_neighbor_reclaims_the_gap_without_repeating_own_setpos() {
        // Explorer's work area can still end at our old reservation after the
        // bar above us closes. The notification must override that kept check.
        let monitor = RECT {
            left: -1920,
            top: 0,
            right: 0,
            bottom: 1080,
        };
        let display = Display {
            id: "display2".into(),
            monitor,
            work: RECT { top: 60, ..monitor },
            dpi: 96,
        };
        // Only mocked shell calls run in this test; no native window is owned.
        let mut reservation = std::mem::ManuallyDrop::new(Reservation {
            hwnd: HWND::default(),
            registered: true,
            strip: RECT {
                top: 28,
                bottom: 60,
                ..monitor
            },
            placed: Some((monitor, 32)),
            retry: 0,
        });
        let mut calls = Vec::new();
        let mut shell = |message, data: &mut APPBARDATA| {
            calls.push(message);
            if message == ABM_QUERYPOS {
                data.rc.top = 0;
            }
            1
        };
        reservation.check_with(&display, 32., false, true, &mut shell);
        assert_eq!(
            reservation.strip,
            RECT {
                top: 0,
                bottom: 32,
                ..monitor
            }
        );
        // Explorer posts ABN_POSCHANGED for our own SETPOS too. Query again,
        // but do not issue another SETPOS that would produce another event.
        // Even if Windows has not yet published the new work-area boundary.
        let pending_work_area = Display {
            id: display.id.clone(),
            monitor,
            work: monitor,
            dpi: 96,
        };
        reservation.check_with(&pending_work_area, 32., false, true, &mut shell);
        // The ordinary timer remains quiet once the reservation is kept.
        reservation.check_with(&display, 32., false, false, &mut shell);
        assert_eq!(calls, [ABM_QUERYPOS, ABM_SETPOS, ABM_QUERYPOS]);
    }
}
