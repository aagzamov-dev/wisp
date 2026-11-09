//! Dock-owned Control Center. Windows queries and mutations stay on the command worker.

use std::{cell::RefCell, collections::VecDeque, time::Instant};

use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Input::KeyboardAndMouse::{ReleaseCapture, SetCapture},
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
const WIDTH: f32 = 424.0;

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
    Wifi,
    Bluetooth,
    Output,
    Mixer,
    Display,
    Edit,
}

#[derive(Clone)]
enum Click {
    Page(Page),
    Action(Action),
    Volume,
    Brightness(control::Display),
    AppVolume(u32),
    Seek(media::Timeline),
    Edit(usize),
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
    fade: Option<Instant>,
}

fn preferences() -> std::path::PathBuf {
    crate::util::data_dir().join("control-center-sections")
}

impl Panel {
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
                h: 76.,
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
            Page::Wifi => {
                self.header("Wi-Fi");
                if let Some(wifi) = self.state.wifi.clone().flatten() {
                    self.row(
                        60.,
                        if wifi.on { "Turn Wi-Fi off" } else { "Turn Wi-Fi on" },
                        "",
                        glyph::WIFI,
                        wifi.on,
                        Some(Click::Action(Action::Run(Command::WifiPower(!wifi.on)))),
                    );
                }
                let mut y = 108.;
                if let Some(networks) = self.hardware.networks.clone() {
                    for network in networks.list {
                        let label = if network.connected {
                            "Connected"
                        } else if network.profile.is_some() {
                            "Saved"
                        } else {
                            "Available"
                        };
                        let action = network
                            .profile
                            .map(|id| Click::Action(Action::Run(Command::WifiConnect(id))))
                            .or(Some(Click::Action(Action::Settings("ms-settings:network-wifi"))));
                        self.row(y, network.name, label, glyph::WIFI, network.connected, action);
                        y += 46.;
                    }
                }
                self.row(
                    y,
                    "Windows Wi-Fi settings",
                    "",
                    "\u{E713}",
                    false,
                    Some(Click::Action(Action::Settings("ms-settings:network-wifi"))),
                );
            }
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
        let wifi = self.state.wifi.clone().flatten();
        let title = wifi
            .as_ref()
            .and_then(|w| w.network.clone())
            .unwrap_or_else(|| "Wi-Fi".into());
        self.tile(
            (0, 14.),
            title,
            wifi.as_ref().map_or("Unavailable", |w| {
                if w.network.is_some() {
                    "Connected"
                } else if w.on {
                    "On"
                } else {
                    "Off"
                }
            }),
            glyph::WIFI,
            wifi.is_some_and(|w| w.on),
            Click::Page(Page::Wifi),
        );
        let bt = self.state.bluetooth.clone().flatten();
        let detail = bt.as_ref().map_or_else(
            || "Unavailable".into(),
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
            (1, 14.),
            "Bluetooth",
            detail,
            glyph::BLUETOOTH,
            bt.is_some_and(|bt| bt.on),
            Click::Page(Page::Bluetooth),
        );
        self.tile(
            (0, 102.),
            "Do not disturb",
            "Windows settings",
            "\u{E708}",
            false,
            Click::Action(Action::Settings("ms-settings:notifications")),
        );
        self.tile(
            (1, 102.),
            "Project",
            "Display mode",
            "\u{E7F4}",
            false,
            Click::Action(Action::Project),
        );
        let mut y = 190.;
        if self.hidden & 2 == 0 {
            self.card(y, 115., "Sound");
            if let Some(audio) = self.state.audio.clone().flatten() {
                self.add(
                    Bounds {
                        x: self.width - 70.,
                        y: y + 9.,
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
                    y + 38.,
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
                        y: y + 75.,
                        w: self.width - 150.,
                        h: 31.,
                    },
                    Style::Row(false),
                    audio.device,
                    output_detail,
                    "",
                    Some(Click::Page(Page::Output)),
                );
                self.add(
                    Bounds {
                        x: self.width - 110.,
                        y: y + 75.,
                        w: 81.,
                        h: 31.,
                    },
                    Style::Row(false),
                    "Mixer",
                    "",
                    "",
                    Some(Click::Page(Page::Mixer)),
                );
            } else {
                self.row(y + 39., "No sound output available", "", "", false, None);
            }
            y += 127.;
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
        let settings_x = self.width - if display { 211. } else { 153. };
        self.add(
            Bounds {
                x: 14.,
                y,
                w: settings_x - 26.,
                h: 35.,
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
                w: 48.,
                h: 35.,
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
                    x: self.width - 153.,
                    y,
                    w: 48.,
                    h: 35.,
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
                x: self.width - 97.,
                y,
                w: 83.,
                h: 35.,
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
        let height = self.content_height().min(self.max_height);
        if (height - self.height).abs() < 1. {
            return;
        }
        self.height = height;
        self.scroll = self.scroll.min(self.max_scroll());
        let scale = self.dpi as f32 / 96.;
        let width = (self.width * scale).round() as i32;
        let height = (height * scale).round() as i32;
        unsafe {
            let mut rect = RECT::default();
            if GetWindowRect(hwnd, &mut rect).is_ok() {
                crate::theme::shape(hwnd, width, height, 30., self.dpi);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    rect.left,
                    self.bottom - height,
                    width,
                    height,
                    SWP_NOACTIVATE | SWP_NOZORDER,
                );
            }
        }
    }

    fn max_scroll(&self) -> f32 {
        (self.content_height() - self.height).max(0.)
    }

    fn activate(&mut self, click: Click, hwnd: HWND, value: Option<u32>) {
        let action = match click {
            Click::Page(page) => {
                self.page = page;
                self.scroll = 0.;
                self.focus = None;
                self.rebuild();
                match page {
                    Page::Wifi => Some(Action::Hardware(control::Request::Networks)),
                    Page::Display => Some(Action::Hardware(control::Request::Displays)),
                    Page::Bluetooth => Some(Action::Hardware(control::Request::Batteries)),
                    _ => None,
                }
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
        };
        if let Some(action) = action {
            self.queue.push_back(action);
            unsafe {
                let _ = PostMessageW(Some(self.launcher), WM_SOUND_ACTION, WPARAM(hwnd.0 as usize), LPARAM(0));
            }
        }
    }

    fn pointer(&self, lp: LPARAM) -> (f32, f32) {
        let scale = 96. / self.dpi as f32;
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
        (((x - b.x - 26.) / (b.w - 39.)).clamp(0., 1.) * 100.).round() as u32
    }
}

fn panel(hwnd: HWND) -> *const RefCell<Panel> {
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<Panel> }
}

pub fn open(
    owner: HWND,
    launcher: HWND,
    anchor: RECT,
    dpi: u32,
    state: State,
    hardware: control::Hardware,
    renderer: Renderer,
) -> Result<HWND> {
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
        let width = ((WIDTH * scale).round() as i32).min(info.rcWork.right - info.rcWork.left - 16);
        let bottom = (anchor.top - 8).min(info.rcWork.bottom - 8);
        let height = bottom - info.rcWork.top - 8;
        if width <= 0 || height <= 0 {
            return Err(Error::from_hresult(windows::Win32::Foundation::E_FAIL));
        }
        let x = (anchor.right - width).clamp(
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
            state,
            hardware,
            renderer,
            palette: Palette::new(true, false),
            page: Page::Home,
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
            fade: None,
        };
        contents.rebuild();
        contents.height = contents.content_height().min(contents.max_height);
        let height = (contents.height * scale).round() as i32;
        let y = bottom - height;
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            class.lpszClassName,
            w!("Control Center"),
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
        SetWindowLongPtrW(
            hwnd,
            GWLP_USERDATA,
            Box::into_raw(Box::new(RefCell::new(contents))) as isize,
        );
        crate::theme::shape(hwnd, width, height, 30., dpi);
        if let Some(cell) = panel(hwnd).as_ref() {
            let fade = super::start_popup_fade(hwnd);
            cell.borrow_mut().fade = fade;
        }
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetTimer(Some(hwnd), REFRESH, 1000, None);
        let _ = PostMessageW(Some(launcher), WM_SOUND_REFRESH, WPARAM(hwnd.0 as usize), LPARAM(1));
        Ok(hwnd)
    }
}

pub fn update(hwnd: HWND, state: &State) {
    if let Some(cell) = unsafe { panel(hwnd).as_ref() }
        && let Ok(mut p) = cell.try_borrow_mut()
        && p.dragging.is_none()
        && p.state != *state
    {
        p.state = state.clone();
        p.rebuild();
        p.fit(hwnd);
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
}

pub fn update_hardware(hwnd: HWND, hardware: control::Hardware) {
    if let Some(cell) = unsafe { panel(hwnd).as_ref() }
        && let Ok(mut p) = cell.try_borrow_mut()
    {
        if p.dragging.is_some() || p.hardware == hardware {
            return;
        }
        p.hardware = hardware;
        p.rebuild();
        p.fit(hwnd);
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
}

pub fn take_action(hwnd: HWND) -> Option<Action> {
    unsafe { panel(hwnd).as_ref() }?
        .try_borrow_mut()
        .ok()?
        .queue
        .pop_front()
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCDESTROY {
            let data = panel(hwnd);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            if !data.is_null() {
                drop(Box::from_raw(data.cast_mut()));
            }
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        if msg == WM_CLOSE {
            let _ = DestroyWindow(hwnd);
            return LRESULT(0);
        }
        let Some(cell) = panel(hwnd).as_ref() else {
            return DefWindowProcW(hwnd, msg, wp, lp);
        };
        let Ok(mut p) = cell.try_borrow_mut() else {
            return DefWindowProcW(hwnd, msg, wp, lp);
        };
        match msg {
            WM_ERASEBKGND => return LRESULT(1),
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut paint);
                let max_scroll = p.max_scroll();
                let Panel {
                    renderer,
                    dpi,
                    palette,
                    elements,
                    scroll,
                    focus,
                    dragging,
                    ..
                } = &mut *p;
                let _ = renderer.draw_control(hwnd, *dpi, palette, elements, *scroll, max_scroll, *focus, *dragging);
                let _ = EndPaint(hwnd, &paint);
                return LRESULT(0);
            }
            WM_TIMER if wp.0 == REFRESH => {
                if p.page == Page::Home && p.elements.iter().any(|e| matches!(e.style, Style::Timeline(_, true))) {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                let _ = PostMessageW(Some(p.launcher), WM_SOUND_REFRESH, WPARAM(hwnd.0 as usize), LPARAM(0));
            }
            WM_TIMER if wp.0 == super::POPUP_FADE_TIMER => {
                super::tick_popup_fade(hwnd, &mut p.fade);
            }
            WM_ACTIVATE if wp.0 & 0xffff == WA_INACTIVE as usize => {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
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
                0x1b => {
                    if p.page == Page::Home {
                        let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                    } else {
                        p.activate(Click::Page(Page::Home), hwnd, None);
                    }
                }
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
            _ => return DefWindowProcW(hwnd, msg, wp, lp),
        }
        if !matches!(msg, WM_TIMER | WM_ACTIVATE) {
            p.fit(hwnd);
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        LRESULT(0)
    }
}
