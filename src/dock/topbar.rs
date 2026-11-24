//! Optional per-monitor chrome. Shares the dock's tray host and command worker.
//! Owned surfaces use per-pixel tints without a native rectangular backdrop.

use super::*;
use crate::commands::{
    Command,
    audio::Spectrum,
    media::{Control, NotchTrack},
};
use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_CONTROL, VK_LEFT, VK_RETURN, VK_RIGHT, VK_SPACE};
#[path = "topbar_model.rs"]
pub mod model;
use model::{Hit, Layout, Spring};

pub const REFRESH: u32 = WM_APP + 44;
pub const ACTION: u32 = WM_APP + 45;
pub const BATTERY_SETTINGS: u32 = WM_APP + 48;
const TICK: usize = 1;
const MOTION: usize = 2;
const DRAG: usize = 3;
const TITLE_TIMEOUT: usize = 4;
const SYNC: u32 = WM_APP + 46;
const MATERIAL: u32 = WM_APP + 49;

#[derive(Clone, PartialEq, Eq)]
pub struct Options {
    pub height: u32,
    pub opacity: u32,
    pub island_opacity: u32,
    pub island_blur: bool,
    pub monitors: Vec<String>,
    pub theme: crate::config::Theme,
    pub backdrop: Backdrop,
    pub backdrop_color: String,
    pub material_options: crate::theme::MaterialOptions,
}
impl Options {
    pub fn from_config(config: &crate::config::Config) -> Self {
        Self {
            height: config.topbar_height.clamp(24, 48),
            opacity: config.topbar_opacity.min(100),
            island_opacity: config.topbar_island_opacity.min(100),
            island_blur: config.topbar_island_blur,
            monitors: config.topbar_monitors.clone(),
            theme: config.theme,
            backdrop: config.backdrop,
            backdrop_color: config.backdrop_color.clone(),
            material_options: config.material_options(),
        }
    }
}

pub struct Bar {
    hwnd: HWND,
    state: Rc<RefCell<BarState>>,
}
pub struct Display {
    pub handle: HMONITOR,
    pub id: String,
    pub primary: bool,
}
pub fn displays() -> Vec<Display> {
    unsafe extern "system" fn collect(handle: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
        unsafe {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            if GetMonitorInfoW(handle, &mut info.monitorInfo).as_bool() {
                let id = String::from_utf16_lossy(
                    &info.szDevice[..info
                        .szDevice
                        .iter()
                        .position(|c| *c == 0)
                        .unwrap_or(info.szDevice.len())],
                );
                (*(lp.0 as *mut Vec<Display>)).push(Display {
                    handle,
                    id,
                    primary: info.monitorInfo.dwFlags & 1 != 0,
                });
            }
            BOOL(1)
        }
    }
    let mut result = Vec::new();
    let _ = unsafe { EnumDisplayMonitors(None, None, Some(collect), LPARAM(&raw mut result as isize)) };
    result
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Preferences {
    icons: HashMap<String, bool>,
    #[serde(default)]
    order: Vec<String>,
}
fn preference_key(icon: &tray_host::Icon) -> Option<String> {
    if let Some(guid) = icon.guid {
        Some(format!("guid:{guid:?}"))
    } else {
        icon.exes
            .first()
            .filter(|s| !s.is_empty())
            .map(|exe| format!("{}:{}", exe.to_lowercase(), icon.uid))
    }
}
impl Preferences {
    fn path() -> std::path::PathBuf {
        crate::util::data_dir().join("topbar-tray.toml")
    }
    fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }
    fn promoted(&self, icon: &tray_host::Icon) -> bool {
        preference_key(icon)
            .and_then(|key| self.icons.get(&key).copied())
            .unwrap_or(icon.promoted == Some(true))
    }
    fn save(&mut self, icon: &tray_host::Icon, promoted: bool) -> std::io::Result<()> {
        let key =
            preference_key(icon).ok_or_else(|| std::io::Error::other("This icon has no persistent app identity"))?;
        let mut next = Self {
            icons: self.icons.clone(),
            order: self.order.clone(),
        };
        next.order.retain(|id| id != &key);
        if promoted {
            next.order.insert(0, key.clone());
        }
        next.icons.insert(key, promoted);
        next.write_to(&Self::path())?;
        *self = next;
        Ok(())
    }
    fn write_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        let temp = path.with_extension("tmp");
        std::fs::write(&temp, toml::to_string(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(temp, path)?;
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct PointerTarget {
    hit: Hit,
    tray_key: Option<tray_host::Key>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TrayRightPress {
    Icon(HWND, tray_host::Key),
    Chevron,
}

struct TrayDrag {
    key: tray_host::Key,
    gesture: model::DragGesture,
}

#[derive(Default)]
struct ActionQueue {
    commands: std::collections::VecDeque<Command>,
    volume: Option<u32>,
    wheel: i32,
}
impl ActionQueue {
    fn sync_volume(&mut self, volume: Option<u32>) {
        if self.commands.is_empty() {
            self.volume = volume;
        }
    }
    fn push(&mut self, command: Command) {
        self.commands.push_back(command);
    }
    fn pop(&mut self) -> Option<Command> {
        self.commands.pop_front()
    }
    fn volume_step(&mut self, delta: i32) -> bool {
        let Some(current) = self.volume else { return false };
        let (next, carry) = logic::wheel_volume(current, self.wheel + delta);
        self.wheel = carry;
        if next == current {
            return false;
        }
        self.volume = Some(next);
        self.push(Command::Volume(next));
        true
    }
}
type IslandGeometry = (i32, i32, i32, i32, u32);

fn bar_panel_current(cell: &RefCell<BarState>, target: PanelTarget) -> bool {
    let current = {
        let s = cell.borrow();
        panel_generation_current(
            (target.epoch, target.material_version),
            (s.panel_epoch, s.material_version),
            s.panel_identity == Some(target.id),
            unsafe { GetWindowLongPtrW(s.hwnd, GWLP_USERDATA) == cell as *const _ as isize },
        )
    };
    current && sound::is_current(target.id)
}

struct BarState {
    hwnd: HWND,
    island: HWND,
    flyout: HWND,
    owner: HWND,
    monitor: HMONITOR,
    device_id: String,
    data: Rc<DockData>,
    options: Options,
    dpi: u32,
    rect: RECT,
    island_geometry: Option<IslandGeometry>,
    blur_host: Option<super::topbar_blur::BlurHost>,
    blur_presented: Option<(IslandGeometry, u8)>,
    visible: bool,
    renderer: render::Renderer,
    island_renderer: render::Renderer,
    flyout_renderer: render::Renderer,
    palette: Palette,
    island_palette: Palette,
    popup_palette: Palette,
    popup_material: Option<crate::theme::SurfaceMaterial>,
    material_version: u64,
    popup_prepared: bool,
    island_prepared: bool,
    island_painted_reveal: f32,
    status: status::Status,
    date_width: f32,
    time_width: f32,
    title_width: f32,
    commands: crate::commands::State,
    hardware: crate::commands::control::Hardware,
    track: Option<NotchTrack>,
    media_exclusions: Vec<String>,
    outgoing_track: Option<NotchTrack>,
    title: model::TrackTitle,
    outgoing_title: bool,
    artwork_color: Option<[f32; 3]>,
    equalizer_color: D2D1_COLOR_F,
    spectrum: Option<Spectrum>,
    equalizer: [f32; model::EQUALIZER_BAR_COUNT],
    animations_enabled: bool,
    track_fade: Spring,
    last_playing: Option<Instant>,
    media_leader: bool,
    width: Spring,
    reveal: Spring,
    content: Spring,
    compact: bool,
    hover_level: Spring,
    press_level: Spring,
    last_frame: Instant,
    motion_running: bool,
    flyout_progress: crate::animation::Motion,
    flyout_closing: bool,
    flyout_base: RECT,
    hover: Option<Hit>,
    down: Option<PointerTarget>,
    tray_right_press: Option<TrayRightPress>,
    focused: Option<Hit>,
    shown: Vec<tray::Shown>,
    inline: Vec<usize>,
    overflow: Vec<usize>,
    preferences: Preferences,
    flyout_open: bool,
    flyout_hover: Option<usize>,
    flyout_press: Option<tray_host::Key>,
    drag: Option<TrayDrag>,
    drag_target: Option<bool>,
    suppress_tray_release: bool,
    panel: HWND,
    panel_identity: Option<sound::Identity>,
    closing_panels: Vec<sound::Identity>,
    panel_entry: sound::Entry,
    panel_epoch: u64,
    pending_panel: Option<(u64, sound::OpenRequest)>,
    actions: ActionQueue,
    tray_menu: Option<(u32, Instant)>,
    native_menu_open: bool,
    last_click: Option<(tray_host::Key, Instant)>,
    fullscreen_at: Option<Instant>,
    last_request: Instant,
}

impl Bar {
    pub(super) fn new(owner: HWND, display: &Display, data: Rc<DockData>, options: Options) -> Result<Self> {
        let renderer = render::Renderer::new()?;
        let state = Rc::new(RefCell::new(BarState {
            hwnd: HWND::default(),
            island: HWND::default(),
            flyout: HWND::default(),
            owner,
            monitor: display.handle,
            device_id: display.id.clone(),
            data,
            options,
            dpi: 96,
            rect: RECT::default(),
            island_geometry: None,
            blur_host: None,
            blur_presented: None,
            visible: false,
            island_renderer: renderer.for_window(),
            flyout_renderer: renderer.for_window(),
            renderer,
            palette: Palette::new(true, true),
            island_palette: Palette::new(true, true),
            popup_palette: Palette::new(true, true),
            status: status::read_clock(),
            date_width: 100.,
            time_width: 48.,
            title_width: 120.,
            commands: crate::commands::State::default(),
            hardware: crate::commands::control::Hardware::default(),
            track: None,
            media_exclusions: Vec::new(),
            outgoing_track: None,
            title: model::TrackTitle::default(),
            outgoing_title: false,
            artwork_color: None,
            equalizer_color: Palette::new(true, true).text,
            spectrum: None,
            equalizer: model::EQUALIZER_IDLE,
            animations_enabled: client_animations_enabled(),
            track_fade: Spring::new(1.),
            last_playing: None,
            media_leader: false,
            width: Spring::new(320.),
            reveal: Spring::new(1.),
            content: Spring::new(0.),
            compact: false,
            hover_level: Spring::new(0.),
            press_level: Spring::new(0.),
            last_frame: Instant::now(),
            motion_running: false,
            flyout_progress: crate::animation::Motion::default(),
            flyout_closing: false,
            flyout_base: RECT::default(),
            hover: None,
            down: None,
            tray_right_press: None,
            focused: None,
            shown: Vec::new(),
            inline: Vec::new(),
            overflow: Vec::new(),
            preferences: Preferences::load(),
            popup_material: None,
            material_version: 0,
            popup_prepared: false,
            island_prepared: false,
            island_painted_reveal: 0.,
            flyout_open: false,
            flyout_hover: None,
            flyout_press: None,
            drag: None,
            drag_target: None,
            suppress_tray_release: false,
            panel: HWND::default(),
            panel_identity: None,
            closing_panels: Vec::new(),
            panel_entry: sound::Entry::Home,
            panel_epoch: 0,
            pending_panel: None,
            actions: ActionQueue::default(),
            tray_menu: None,
            native_menu_open: false,
            last_click: None,
            fullscreen_at: None,
            last_request: Instant::now() - Duration::from_secs(2),
        }));
        unsafe {
            let instance = GetModuleHandleW(None)?.into();
            let class = WNDCLASSW {
                hInstance: instance,
                lpfnWndProc: Some(proc),
                lpszClassName: w!("Wisp.TopBar"),
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
                return Err(Error::from_thread());
            }
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_LAYERED,
                class.lpszClassName,
                w!("Wisp top bar"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                Some(owner),
                None,
                Some(instance),
                Some(Rc::as_ptr(&state).cast()),
            )?;
            state.borrow_mut().hwnd = hwnd;
            let bar = Self { hwnd, state };
            let island = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_LAYERED,
                class.lpszClassName,
                w!("Wisp activity island"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                Some(hwnd),
                None,
                Some(instance),
                Some(Rc::as_ptr(&bar.state).cast()),
            )?;
            bar.state.borrow_mut().island = island;
            let flyout = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_LAYERED,
                class.lpszClassName,
                w!("Hidden notification icons"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                Some(hwnd),
                None,
                Some(instance),
                Some(Rc::as_ptr(&bar.state).cast()),
            )?;
            bar.state.borrow_mut().flyout = flyout;
            BARS.with_borrow_mut(|bars| bars.push(hwnd));
            bar.state.borrow_mut().material();
            bar.state.borrow_mut().measure_content();
            {
                let mut s = bar.state.borrow_mut();
                s.width = Spring::new(s.date_width + s.time_width + 38.);
            }
            SetTimer(Some(hwnd), TICK, 500, None);
            bar.state.borrow_mut().poll();
            Ok(bar)
        }
    }
    pub fn monitor(&self) -> HMONITOR {
        self.state.borrow().monitor
    }
    pub fn id(&self) -> usize {
        self.hwnd.0 as usize
    }
    pub fn visible(&self) -> bool {
        self.state.borrow().visible
    }
    pub fn update_status(
        &self,
        audio: Option<crate::commands::audio::State>,
        wifi: Option<crate::commands::wifi::State>,
    ) {
        let mut s = self.state.borrow_mut();
        let changed = s.commands.audio != Some(audio.clone()) || s.commands.wifi != Some(wifi.clone());
        s.actions.sync_volume(audio.as_ref().map(|audio| audio.volume));
        s.commands.audio = Some(audio);
        s.commands.wifi = Some(wifi);
        if changed {
            s.invalidate();
        }
    }
    fn hide_companions(&self) {
        let (island, popup) = {
            let mut s = self.state.borrow_mut();
            (s.blur_host.take(), s.popup_material.take())
        };
        if let Some(host) = &island {
            host.hide();
        }
        if let Some(host) = &popup {
            host.hide();
        }
        let mut s = self.state.borrow_mut();
        s.blur_host = island;
        s.blur_presented = None;
        s.popup_material = popup;
    }
    pub fn configure(&self, options: Options, leader: bool) {
        if self.state.borrow().options != options {
            self.hide_companions();
        }
        let mut s = self.state.borrow_mut();
        s.media_leader = leader;
        if s.options != options {
            s.options = options;
            s.material();
            s.place();
        }
        s.request();
    }
    pub fn set_media_exclusions(&self, exclusions: &[String]) {
        if self.state.borrow().media_exclusions != exclusions {
            self.hide_companions();
        }
        let mut s = self.state.borrow_mut();
        if s.media_exclusions == exclusions {
            return;
        }
        s.media_exclusions = exclusions.to_vec();
        s.actions
            .commands
            .retain(|command| !matches!(command, Command::Media(_)));
        let excluded =
            |track: &NotchTrack| crate::commands::media::excluded(&track.media.source, &track.media.app, exclusions);
        if s.track.as_ref().is_some_and(excluded) {
            s.track = None;
            s.last_playing = None;
            s.reset_title(false);
            s.artwork_color = None;
            s.update_equalizer_color();
        }
        if s.outgoing_track.as_ref().is_some_and(excluded) {
            s.outgoing_track = None;
        }
        s.island_renderer.clear_media_artwork();
        s.measure_content();
        s.media_state();
        s.invalidate();
    }
    pub fn update_track(&self, track: Option<NotchTrack>) {
        let mut s = self.state.borrow_mut();
        let track = track.filter(|track| {
            !crate::commands::media::excluded(&track.media.source, &track.media.app, &s.media_exclusions)
        });
        let changed = match (&s.track, &track) {
            (None, None) => false,
            (Some(old), Some(new)) => {
                old.media.source != new.media.source
                    || old.media.title != new.media.title
                    || old.media.artist != new.media.artist
                    || old.media.playing != new.media.playing
                    || match (&old.artwork, &new.artwork) {
                        (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                        (None, None) => false,
                        _ => true,
                    }
            }
            _ => true,
        };
        if changed {
            drop(s);
            self.hide_companions();
            s = self.state.borrow_mut();
        }
        let metadata_changed = track_metadata_changed(s.track.as_ref(), track.as_ref());
        if metadata_changed || s.track.is_some() != track.is_some() {
            s.outgoing_title = s.title.deadline.is_some_and(|deadline| Instant::now() < deadline);
            s.reset_title(track.is_some());
        }
        let artwork = |track: &NotchTrack| track.artwork.as_ref().map(Arc::as_ptr);
        if s.track.as_ref().and_then(artwork) != track.as_ref().and_then(artwork) {
            s.artwork_color = track.as_ref().and_then(|track| {
                track
                    .artwork
                    .as_ref()
                    .and_then(|pixels| model::artwork_color(&pixels.bgra))
            });
            s.update_equalizer_color();
        }
        if (track.is_none() || metadata_changed) && s.track.is_some() {
            s.outgoing_track = s.track.take();
        }
        s.track = track;
        if changed {
            s.measure_content();
        }
        if metadata_changed {
            s.track_fade.value = 0.;
            s.track_fade.velocity = 0.;
            s.track_fade.target = 1.;
        }
        s.media_state();
        if changed {
            s.invalidate_surfaces(false, true);
        }
    }
    pub fn take_action(&self) -> Option<Command> {
        self.state.borrow_mut().actions.pop()
    }
    pub fn panel(&self, id: usize) -> Option<HWND> {
        let s = self.state.borrow();
        (s.panel.0 as usize == id).then(|| s.current_panel()).flatten()
    }
    pub fn confirm_wifi(&self, on: bool) {
        let target = {
            let s = self.state.borrow();
            s.panel_identity.map(|id| PanelTarget {
                id,
                epoch: s.panel_epoch,
                material_version: s.material_version,
            })
        };
        if let Some(target) = target.filter(|target| bar_panel_current(&self.state, *target)) {
            sound::confirm_wifi(target.id.hwnd, on);
        }
    }
    pub fn update_commands(&self, state: &crate::commands::State) {
        unborrowed_action(
            &self.state,
            |s| {
                let changed = s.commands.audio != state.audio || s.commands.wifi != state.wifi;
                s.commands = state.clone();
                s.actions
                    .sync_volume(state.audio.as_ref().and_then(|a| a.as_ref()).map(|a| a.volume));
                if changed {
                    s.invalidate();
                }
                s.panel_identity.map(|id| PanelTarget {
                    id,
                    epoch: s.panel_epoch,
                    material_version: s.material_version,
                })
            },
            |target| {
                if let Some(target) = target.filter(|target| bar_panel_current(&self.state, *target)) {
                    sound::update(target.id.hwnd, state);
                }
            },
        );
    }
    pub fn update_hardware(&self, hardware: crate::commands::control::Hardware) {
        let panel_hardware = hardware.clone();
        unborrowed_action(
            &self.state,
            |s| {
                s.hardware = hardware;
                s.panel_identity.map(|id| PanelTarget {
                    id,
                    epoch: s.panel_epoch,
                    material_version: s.material_version,
                })
            },
            |target| {
                if let Some(target) = target.filter(|target| bar_panel_current(&self.state, *target)) {
                    sound::update_hardware(target.id.hwnd, panel_hardware);
                }
            },
        );
    }
}
impl Drop for Bar {
    fn drop(&mut self) {
        let hosts = {
            let mut s = self.state.borrow_mut();
            s.material_version = s.material_version.wrapping_add(1);
            (s.blur_host.take(), s.popup_material.take())
        };
        drop(hosts);
        let mut s = self.state.borrow_mut();
        s.close_panel();
        unsafe {
            if GetCapture() == s.hwnd || GetCapture() == s.flyout {
                let _ = ReleaseCapture();
            }
            for hwnd in [s.hwnd, s.island, s.flyout] {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            let _ = DestroyWindow(s.hwnd);
        }
    }
}

impl BarState {
    fn scale(&self) -> f32 {
        self.dpi as f32 / 96.
    }
    fn layout(&self) -> Layout {
        Layout {
            width: (self.rect.right - self.rect.left) as f32 / self.scale(),
            height: self.options.height as f32,
            inline: self.inline.len(),
            battery: self.status.battery.is_some(),
        }
    }
    fn material(&mut self) {
        self.material_version = self.material_version.wrapping_add(1);
        self.popup_prepared = false;
        self.island_prepared = false;
        self.blur_presented = None;
        self.palette = crate::theme::apply_topbar(self.hwnd, self.options.opacity, false);
        self.island_palette = crate::theme::apply_topbar(self.island, self.options.island_opacity, true);
        self.update_equalizer_color();
        let dark = match self.options.theme {
            crate::config::Theme::Dark => true,
            crate::config::Theme::Light => false,
            crate::config::Theme::System => !crate::system::system_uses_light_theme(),
        };
        self.popup_palette = crate::theme::apply_layered_surface(
            self.flyout,
            dark,
            self.options.backdrop,
            &self.options.backdrop_color,
            self.options.material_options,
        );
        let _ = unsafe { PostMessageW(Some(self.hwnd), MATERIAL, WPARAM(0), LPARAM(0)) };
        self.invalidate();
    }
    fn invalidate(&self) {
        self.invalidate_surfaces(true, true);
    }
    fn invalidate_surfaces(&self, bar: bool, island: bool) {
        unsafe {
            if bar {
                let _ = InvalidateRect(Some(self.hwnd), None, false);
            }
            if island {
                let _ = InvalidateRect(Some(self.island), None, false);
            }
        }
    }
    fn request(&mut self) {
        if self.media_leader && self.last_request.elapsed() >= Duration::from_millis(900) {
            self.last_request = Instant::now();
            let _ = unsafe { PostMessageW(Some(self.owner), REFRESH, WPARAM(self.hwnd.0 as usize), LPARAM(0)) };
        }
    }
    fn surface_order(&self) -> (Option<HWND>, SET_WINDOW_POS_FLAGS) {
        let order = super::dock_menu_order(self.native_menu_open, self.tray_menu.is_some());
        ((order != SWP_NOZORDER).then_some(HWND_TOPMOST), SWP_NOACTIVATE | order)
    }
    fn place(&mut self) {
        let Some(info) = monitor_info(self.monitor) else { return };
        let reserved_top = self
            .data
            .topbar_strips
            .borrow()
            .as_ref()
            .and_then(|strips| super::topbar_space::strip(strips, &self.device_id))
            .map_or(info.rcMonitor.top, |rect| rect.top);
        unsafe {
            if MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONULL) != self.monitor {
                let _ = SetWindowPos(
                    self.hwnd,
                    None,
                    info.rcMonitor.left,
                    info.rcMonitor.top,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            self.dpi = GetDpiForWindow(self.hwnd).max(96);
            let h = (self.options.height as f32 * self.scale()).round() as i32;
            self.rect = RECT {
                left: info.rcMonitor.left,
                top: reserved_top,
                right: info.rcMonitor.right,
                bottom: reserved_top + h,
            };
            let (order, flags) = self.surface_order();
            let _ = SetWindowPos(
                self.hwnd,
                order,
                self.rect.left,
                self.rect.top,
                self.rect.right - self.rect.left,
                h,
                flags,
            );
            self.place_island();
        }
        self.sync_tray();
        self.partition();
    }
    fn place_island(&mut self) {
        let scale = self.scale();
        let reveal = self.reveal.value.clamp(0., 1.);
        let width = (self.width.value * (0.94 + 0.06 * reveal) * scale).round() as i32;
        let height = ((self.options.height as f32 - 4.) * (0.85 + 0.15 * reveal) * scale).round() as i32;
        let x = (self.rect.left + self.rect.right - width) / 2;
        let y = self.rect.top + ((2. - (1. - reveal) * 4.) * scale).round() as i32;
        let geometry = (x, y, width, height, self.dpi);
        if self.island_geometry == Some(geometry) {
            return;
        }
        self.blur_presented = None;
        self.island_prepared = false;
        self.island_geometry = Some(geometry);
        unsafe {
            let (order, flags) = self.surface_order();
            let _ = SetWindowPos(self.island, order, x, y, width, height, flags);
        }
        self.sync_panel_opener();
    }
    fn island_width(&self) -> f32 {
        self.island_geometry
            .map_or(self.width.value, |(_, _, width, _, _)| width as f32 / self.scale())
    }
    /// Publish foreground pixels in the same UI callback as geometry updates.
    /// The wrapper synchronizes the retained blur after this borrow is released.
    fn present_island(&mut self) -> Result<()> {
        let reveal = self.reveal.value.clamp(0., 1.);
        let track = self.track.as_ref().or(self.outgoing_track.as_ref());
        let playing = self.media_playing();
        let equalizer = if self.animations_enabled && playing {
            self.equalizer
        } else {
            model::equalizer_bars(playing, None)
        };
        let result = self.island_renderer.draw_topbar_island(
            self.island,
            render::IslandFrame {
                dpi: self.dpi,
                palette: &self.island_palette,
                status: &self.status,
                track,
                previous_track: if self.track.is_some() {
                    self.outgoing_track.as_ref()
                } else {
                    None
                },
                track_fade: self.track_fade.value,
                show_title: if self.track.is_some() {
                    self.title.deadline.is_some()
                } else {
                    self.outgoing_title
                },
                show_previous_title: self.outgoing_title,
                equalizer,
                equalizer_color: self.equalizer_color,
                mix: self.content.value,
                width: self.island_width(),
                reveal,
                compact: self.compact,
                date_width: self.date_width,
                time_width: self.time_width,
                hover: self.hover,
                hover_level: self.hover_level.value,
                press: self.press_level.value,
            },
        );
        self.island_prepared = result.is_ok();
        if result.is_ok() {
            self.sync_island_blur(reveal);
            unsafe {
                let _ = ValidateRect(Some(self.island), None);
            }
        } else {
            self.stop_spectrum();
            self.blur_presented = None;
        }
        result
    }
    fn sync_island_blur(&mut self, reveal: f32) {
        self.island_prepared = true;
        self.island_painted_reveal = reveal;
    }
    fn music(&self) -> bool {
        model::music_visible(self.track.is_some(), self.last_playing.map(|at| at.elapsed()))
    }
    fn media_playing(&self) -> bool {
        self.track.as_ref().is_some_and(|track| track.media.playing)
    }
    fn reset_title(&mut self, has_track: bool) {
        if self.title.deadline.is_some() {
            let _ = unsafe { KillTimer(Some(self.hwnd), TITLE_TIMEOUT) };
        }
        self.title.reset(has_track);
    }
    fn update_equalizer_color(&mut self) {
        let palette = &self.island_palette;
        self.equalizer_color = palette.text;
        if let Some([r, g, b]) = self.artwork_color
            && !crate::theme::high_contrast()
        {
            let tint = D2D1_COLOR_F {
                r: 0.45 * r + 0.55 * palette.text.r,
                g: 0.45 * g + 0.55 * palette.text.g,
                b: 0.45 * b + 0.55 * palette.text.b,
                a: 1.,
            };
            let luminance = |color: D2D1_COLOR_F| {
                let srgb = |x: f32| {
                    if x <= 0.04045 {
                        x / 12.92
                    } else {
                        ((x + 0.055) / 1.055).powf(2.4)
                    }
                };
                0.2126 * srgb(color.r) + 0.7152 * srgb(color.g) + 0.0722 * srgb(color.b)
            };
            let (fg, bg) = (luminance(tint), luminance(palette.background));
            if (fg.max(bg) + 0.05) / (fg.min(bg) + 0.05) >= 3. {
                self.equalizer_color = tint;
            }
        }
    }
    fn media_state(&mut self) {
        let now = Instant::now();
        if self.media_playing() {
            self.last_playing = Some(now);
        }
        let music = self.music();
        if self.title.refresh(now, self.visible && music) {
            if self.title.deadline.is_some() {
                let _ = unsafe {
                    SetTimer(
                        Some(self.hwnd),
                        TITLE_TIMEOUT,
                        model::TITLE_DURATION.as_millis() as u32,
                        None,
                    )
                };
            } else {
                let _ = unsafe { KillTimer(Some(self.hwnd), TITLE_TIMEOUT) };
            }
            self.invalidate_surfaces(false, true);
        }
        self.content.target = if music { 1. } else { 0. };
        let layout = self.layout();
        let side = (layout.inline as f32 * 28. + 40.).max(if layout.battery { 164. } else { 116. });
        let compact = (layout.width - 2. * side - 24.) < 320.;
        let changed_mode = self.compact != compact;
        self.compact = compact;
        let desired = if music {
            model::IslandLayout::music_width(
                compact,
                self.time_width,
                self.track.as_ref().is_some_and(|track| track.artwork.is_some()),
                self.title.deadline.map(|_| self.title_width),
            )
        } else {
            self.date_width + self.time_width + 38.
        };
        self.width.target = desired.min((layout.width - 2. * side - 24.).max(208.));
        if changed_mode {
            self.width.settle();
        }
        if self.visible && self.animations_enabled && self.media_playing() {
            if self.island_prepared && self.spectrum.is_none() {
                self.spectrum = Spectrum::subscribe();
            }
        } else {
            self.stop_spectrum();
        }
        self.animate();
    }
    fn measure_content(&mut self) {
        let title = self.track.as_ref().map_or("", |track| {
            if track.media.title.is_empty() {
                track.media.app.as_str()
            } else {
                track.media.title.as_str()
            }
        });
        if let Ok((date, time, title)) =
            self.island_renderer
                .measure_topbar(&self.status.date, &self.status.time, title)
        {
            self.date_width = date;
            self.time_width = time.max(32.);
            self.title_width = title.clamp(40., 160.);
        }
    }
    fn settle_motion(&mut self) {
        for spring in [
            &mut self.width,
            &mut self.content,
            &mut self.hover_level,
            &mut self.press_level,
            &mut self.track_fade,
            &mut self.reveal,
        ] {
            spring.settle();
        }
        self.flyout_progress.settle();
        self.motion_running = false;
        let _ = unsafe { KillTimer(Some(self.hwnd), MOTION) };
        if self.content.value == 0. || self.track_fade.value == 1. {
            self.outgoing_track = None;
        }
        self.place_island();
        self.present_flyout_motion();
        self.invalidate();
    }
    fn stop_spectrum(&mut self) {
        self.spectrum = None;
        self.equalizer = model::EQUALIZER_IDLE;
    }
    fn visible_spectrum(&self) -> Option<&Spectrum> {
        self.spectrum.as_ref().filter(|_| {
            self.island_prepared
                && self.content.value > 0.
                && model::IslandLayout::new(self.island_width(), self.compact, self.time_width)
                    .media(false)
                    .equalizer_left
                    .is_some()
        })
    }
    fn animate(&mut self) {
        if [
            &self.width,
            &self.content,
            &self.hover_level,
            &self.press_level,
            &self.track_fade,
            &self.reveal,
        ]
        .iter()
        .all(|s| s.value == s.target && s.velocity == 0.)
            && !self.flyout_progress.active()
            && self.visible_spectrum().is_none()
        {
            return;
        }
        if !self.visible || !self.animations_enabled {
            self.settle_motion();
            return;
        }
        if self.motion_running {
            return;
        }
        self.last_frame = Instant::now();
        self.motion_running = unsafe { SetTimer(Some(self.hwnd), MOTION, crate::animation::LOCAL_INTERVAL, None) } != 0;
        if !self.motion_running {
            self.stop_spectrum();
            self.settle_motion();
        }
    }
    fn frame(&mut self) {
        if !self.visible || !self.animations_enabled {
            self.settle_motion();
            return;
        }
        let island_before = (
            self.width.value,
            self.content.value,
            self.track_fade.value,
            self.reveal.value,
        );
        let interaction_before = (self.hover_level.value, self.press_level.value);
        let flyout_before = self.flyout_progress.value;
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32();
        self.last_frame = now;
        let active = self.width.tick_outline(dt)
            | self.content.tick(dt)
            | self.hover_level.tick(dt)
            | self.press_level.tick(dt)
            | self.track_fade.tick(dt)
            | self.reveal.tick(dt)
            | self.flyout_progress.tick(dt);
        if self.content.value == 0. || self.track.is_some() && self.track_fade.value == 1. {
            self.outgoing_track = None;
        }
        let mut equalizer_changed = false;
        if let Some(spectrum) = self.visible_spectrum() {
            let bars = model::equalizer_bars(true, Some(spectrum.levels()));
            equalizer_changed = self.equalizer != bars;
            self.equalizer = bars;
        }
        self.place_island();
        if self.flyout_open && self.flyout_progress.value != flyout_before {
            self.present_flyout_motion();
        }
        let island_changed = island_before
            != (
                self.width.value,
                self.content.value,
                self.track_fade.value,
                self.reveal.value,
            );
        let interaction_changed = interaction_before != (self.hover_level.value, self.press_level.value);
        let (bar_feedback, island_feedback) =
            model::interaction_surfaces(self.hover.or(self.down.map(|target| target.hit)));
        if (island_changed || equalizer_changed || interaction_changed && island_feedback)
            && let Err(error) = self.present_island()
        {
            eprintln!("Top bar island frame: {error}");
        }
        self.invalidate_surfaces(interaction_changed && bar_feedback, false);
        if !active && self.visible_spectrum().is_none() {
            self.motion_running = false;
            let _ = unsafe { KillTimer(Some(self.hwnd), MOTION) };
        }
    }
    fn poll(&mut self) {
        self.poll_tray_menu();
        let reserved = self
            .data
            .topbar_strips
            .borrow()
            .as_ref()
            .and_then(|strips| super::topbar_space::strip(strips, &self.device_id));
        if reserved.is_some_and(|rect| rect.top != self.rect.top) {
            self.place();
        }
        if let Some(info) = monitor_info(self.monitor) {
            if fullscreen_on(&surface_windows(), info.rcMonitor, info.rcWork) {
                self.fullscreen_at = Some(Instant::now());
            }
            let visible = !self
                .fullscreen_at
                .is_some_and(|at| at.elapsed() < Duration::from_millis(600));
            if visible != self.visible {
                self.visible = visible;
                if visible {
                    self.reveal = Spring::new(0.);
                    self.reveal.target = 1.;
                    self.place();
                    self.animate();
                } else {
                    self.stop_spectrum();
                    self.island_prepared = false;
                    self.close_panel();
                    self.close_flyout();
                    self.cancel_drag();
                }
                unsafe {
                    let _ = ShowWindow(self.hwnd, if visible { SW_SHOWNOACTIVATE } else { SW_HIDE });
                    let _ = ShowWindow(self.island, if visible { SW_SHOWNOACTIVATE } else { SW_HIDE });
                }
            }
        }
        self.request();
        if !self.visible {
            return;
        }
        let next = status::read_clock();
        if next != self.status {
            self.status = next;
            self.measure_content();
            self.invalidate();
        }
        self.sync_tray();
        self.media_state();
    }
    fn poll_tray_menu(&mut self) {
        if self.native_menu_open {
            return;
        }
        let Some((thread, started)) = self.tray_menu else {
            return;
        };
        if started.elapsed() < Duration::from_millis(500) {
            return;
        }
        let mut info = GUITHREADINFO {
            cbSize: size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        let menu_flags = GUI_INMENUMODE | GUI_SYSTEMMENUMODE | GUI_POPUPMENUMODE;
        if unsafe { GetGUIThreadInfo(thread, &mut info) }.is_ok()
            && (info.flags.0 & menu_flags.0 != 0 || !info.hwndCapture.is_invalid())
        {
            return;
        }
        self.tray_menu = None;
        self.set_tray_order(HWND_TOPMOST);
    }
    fn set_tray_order(&self, order: HWND) {
        // The island, overflow and their material belong to this window's owner group.
        let _ = unsafe {
            SetWindowPos(
                self.hwnd,
                Some(order),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
        };
    }
    fn sync_tray(&mut self) {
        let icons = self
            .data
            .tray
            .borrow()
            .as_ref()
            .map(TrayHost::icons)
            .unwrap_or_default();
        let same = icons
            .iter()
            .filter(|i| tray::wanted(i))
            .map(|i| (i.key(), i.revision))
            .eq(self.shown.iter().map(|i| (i.icon.key(), i.icon.revision)));
        let wrong_dpi = self.shown.iter().any(|s| {
            s.pixels
                .as_ref()
                .is_some_and(|p| p.width != tray::icon_size(self.dpi) as u32)
        });
        if same && !wrong_dpi {
            self.publish();
            return;
        }
        self.shown = tray::update(std::mem::take(&mut self.shown), icons, tray::icon_size(self.dpi)).0;
        self.partition();
        self.invalidate();
    }
    fn partition(&mut self) {
        let capacity = self.layout().inline_capacity().min(12);
        self.inline = self
            .shown
            .iter()
            .enumerate()
            .filter(|(_, s)| self.preferences.promoted(&s.icon))
            .map(|(i, _)| i)
            .collect();
        self.inline.sort_by_key(|&i| {
            preference_key(&self.shown[i].icon)
                .and_then(|key| self.preferences.order.iter().position(|id| id == &key))
                .unwrap_or(usize::MAX)
        });
        self.inline.truncate(capacity);
        self.overflow = (0..self.shown.len()).filter(|i| !self.inline.contains(i)).collect();
        if self.flyout_open {
            self.place_flyout();
        }
        self.publish();
    }
    fn screen_rect(&self, hit: Hit) -> RECT {
        let l = self.layout();
        let (x, w) = match hit {
            Hit::Tray(i) => (12. + i as f32 * 28., 28.),
            Hit::Overflow => (12. + l.inline as f32 * 28., 28.),
            Hit::Wifi => (l.right(), 32.),
            Hit::Sound => (l.right() + 32., 32.),
            Hit::Battery => (l.right() + 64., 48.),
            Hit::Controls => (l.width - 52., 40.),
            _ => (l.width / 2. - 40., 80.),
        };
        let s = self.scale();
        RECT {
            left: self.rect.left + (x * s).round() as i32,
            right: self.rect.left + ((x + w) * s).round() as i32,
            top: self.rect.top,
            bottom: self.rect.bottom,
        }
    }
    fn publish(&self) {
        self.sync_panel_opener();
        if self.data.tray_anchor.get() != self.hwnd {
            return;
        }
        let tray = self.data.tray.borrow();
        let Some(host) = &*tray else { return };
        let mut rects = tray_host::Rects {
            fallback: Some(self.screen_rect(Hit::Overflow)),
            ..Default::default()
        };
        for (i, &index) in self.inline.iter().enumerate() {
            rects
                .icons
                .insert(self.shown[index].icon.key(), self.screen_rect(Hit::Tray(i)));
        }
        if self.flyout_open {
            for (i, &index) in self.overflow.iter().enumerate() {
                rects.icons.insert(self.shown[index].icon.key(), self.flyout_rect(i));
            }
        }
        host.set_rects(rects);
    }
    fn place_flyout(&mut self) {
        let (w, h) = logic::flyout_size(self.overflow.len());
        let s = self.scale();
        let anchor = self.screen_rect(Hit::Overflow);
        let width = (w * s).round() as i32;
        let height = (h * s).round() as i32;
        let x = (anchor.left - width / 2).clamp(
            self.rect.left + 8,
            (self.rect.right - width - 8).max(self.rect.left + 8),
        );
        self.flyout_base = RECT {
            left: x,
            top: self.rect.bottom + (8. * s).round() as i32,
            right: x + width,
            bottom: self.rect.bottom + (8. * s).round() as i32 + height,
        };
        let visual = model::surface_visual(self.flyout_progress.value);
        let anchor = (anchor.left + anchor.right) / 2;
        unsafe {
            let (order, flags) = self.surface_order();
            let _ = SetWindowPos(
                self.flyout,
                order,
                x + ((anchor - x) as f32 * (1. - visual.scale)).round() as i32,
                self.flyout_base.top - (visual.rise * s).round() as i32,
                (width as f32 * visual.scale).round() as i32,
                (height as f32 * visual.scale).round() as i32,
                flags,
            );
            let _ = InvalidateRect(Some(self.flyout), None, false);
        }
    }
    fn close_flyout(&mut self) {
        if matches!(self.tray_right_press, Some(TrayRightPress::Icon(window, _)) if window == self.flyout) {
            self.tray_right_press = None;
        }
        self.popup_prepared = false;
        self.flyout_open = false;
        self.flyout_closing = false;
        self.flyout_progress = crate::animation::Motion::default();
        self.flyout_hover = None;
        self.flyout_press = None;
        let _ = unsafe { ShowWindow(self.flyout, SW_HIDE) };
        self.publish();
    }
    fn dismiss_flyout(&mut self) {
        if !self.flyout_open {
            return;
        }
        if !client_animations_enabled() || self.flyout_progress.value == 0. {
            self.close_flyout();
            return;
        }
        self.flyout_closing = true;
        if matches!(self.tray_right_press, Some(TrayRightPress::Icon(window, _)) if window == self.flyout) {
            self.tray_right_press = None;
        }
        self.flyout_press = None;
        self.flyout_progress.retarget_duration(false, 140);
        self.animate();
    }
    fn present_flyout_motion(&mut self) {
        if !self.flyout_open {
            return;
        }
        if self.flyout_closing && self.flyout_progress.value == 0. {
            self.close_flyout();
            return;
        }
        let visual = model::surface_visual(self.flyout_progress.value);
        let b = self.flyout_base;
        let anchor = self.screen_rect(Hit::Overflow);
        let anchor = (anchor.left + anchor.right) / 2;
        let rect = RECT {
            left: b.left + ((anchor - b.left) as f32 * (1. - visual.scale)).round() as i32,
            top: b.top - (visual.rise * self.scale()).round() as i32,
            right: b.right,
            bottom: b.top - (visual.rise * self.scale()).round() as i32 + b.bottom - b.top,
        };
        unsafe {
            let mut actual = RECT::default();
            if GetWindowRect(self.flyout, &mut actual).is_err() || actual != rect {
                let _ = SetWindowPos(
                    self.flyout,
                    None,
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOACTIVATE | SWP_NOZORDER,
                );
            }
        }
        let alpha = (visual.opacity.clamp(0., 1.) * 255.).round() as u8;
        let cached = self.popup_prepared
            && !unsafe { GetUpdateRect(self.flyout, None, false) }.as_bool()
            && self
                .flyout_renderer
                .cached_popup_matches(self.flyout, self.dpi)
                .unwrap_or(false);
        let prepared = if cached {
            self.flyout_renderer.present_cached_popup(self.flyout, self.dpi, alpha)
        } else {
            self.flyout_renderer.draw_topbar_flyout(
                self.flyout,
                render::TopbarFlyoutFrame {
                    dpi: self.dpi,
                    palette: &self.popup_palette,
                    shown: &self.shown,
                    overflow: &self.overflow,
                    hover: self.flyout_hover,
                    presentation: (visual.scale, visual.opacity),
                },
            )
        };
        self.popup_prepared = prepared.is_ok();
        self.publish();
    }
    fn show_flyout(&mut self, activate: bool) {
        if !self.flyout_open {
            self.flyout_progress = crate::animation::Motion::default();
        }
        self.flyout_closing = false;
        self.flyout_progress
            .retarget_duration(true, if client_animations_enabled() { 180 } else { 0 });
        self.place_flyout();
        // ShowWindow may synchronously validate WM_PAINT while this state is
        // borrowed by the click handler. Supply the ULW bitmap before showing.
        let visual = model::surface_visual(self.flyout_progress.value);
        let prepared = self.flyout_renderer.draw_topbar_flyout(
            self.flyout,
            render::TopbarFlyoutFrame {
                dpi: self.dpi,
                palette: &self.popup_palette,
                shown: &self.shown,
                overflow: &self.overflow,
                hover: self.flyout_hover,
                presentation: (visual.scale, visual.opacity),
            },
        );
        self.popup_prepared = prepared.is_ok();
        let result = present_then_show(prepared, || {
            self.flyout_open = true;
            self.data.tray_anchor.set(self.hwnd);
            self.publish();
            unsafe {
                let _ = ShowWindow(self.flyout, if activate { SW_SHOW } else { SW_SHOWNOACTIVATE });
                if activate {
                    let _ = SetForegroundWindow(self.flyout);
                }
                let _ = InvalidateRect(Some(self.flyout), None, false);
            }
        });
        if result.is_ok() {
            self.animate();
        }
        if let Err(error) = result {
            eprintln!("Top bar overflow: {error}");
        }
    }
    fn toggle_flyout(&mut self) {
        if self.flyout_open && !self.flyout_closing {
            self.dismiss_flyout();
            return;
        }
        self.close_panel();
        self.show_flyout(true);
    }
    fn flyout_rect(&self, index: usize) -> RECT {
        let mut r = RECT::default();
        let _ = unsafe { GetWindowRect(self.flyout, &mut r) };
        let (x, y) = logic::flyout_cell(index);
        let s = self.scale() * model::surface_visual(self.flyout_progress.value).scale;
        RECT {
            left: r.left + (x * s).round() as i32,
            top: r.top + (y * s).round() as i32,
            right: r.left + ((x + 40.) * s).round() as i32,
            bottom: r.top + ((y + 40.) * s).round() as i32,
        }
    }
    fn sync_panel_opener(&self) {
        if let Some(id) = self.panel_identity {
            let hit = match self.panel_entry {
                sound::Entry::Home => Hit::Controls,
                sound::Entry::Wifi => Hit::Wifi,
                sound::Entry::Sound => Hit::Sound,
                sound::Entry::Calendar => Hit::Clock,
            };
            sound::update_opener(id, self.screen_rect(hit));
        }
    }
    fn current_panel(&self) -> Option<HWND> {
        self.panel_identity
            .filter(|id| sound::is_current(*id))
            .map(|id| id.hwnd)
    }
    fn close_panel(&mut self) {
        self.panel_epoch = self.panel_epoch.wrapping_add(1);
        self.pending_panel = None;
        if let Some(id) = self.panel_identity.take() {
            sound::defer_destroy(id);
        }
        for id in self.closing_panels.drain(..) {
            sound::defer_destroy(id);
        }
        self.panel = HWND::default();
    }
    fn dismiss_panel(&mut self) {
        self.panel_epoch = self.panel_epoch.wrapping_add(1);
        self.pending_panel = None;
        self.closing_panels.retain(|id| sound::is_current(*id));
        if let Some(id) = self.panel_identity.take()
            && sound::is_current(id)
        {
            sound::dismiss(id);
            self.closing_panels.push(id);
        }
        self.panel = HWND::default();
    }
    fn open_panel(&mut self, hit: Hit, entry: sound::Entry) {
        if self.current_panel().is_some() && self.panel_entry == entry {
            self.dismiss_panel();
            return;
        }
        self.dismiss_panel();
        self.close_flyout();
        self.panel_entry = entry;
        self.pending_panel = Some((
            self.panel_epoch,
            sound::OpenRequest {
                placement: sound::Placement {
                    owner: self.hwnd,
                    launcher: self.owner,
                    anchor: self.screen_rect(hit),
                    dpi: self.dpi,
                    entry,
                    below: true,
                },
                state: self.commands.clone(),
                hardware: self.hardware.clone(),
                renderer: self.flyout_renderer.for_window(),
                material: (
                    self.popup_palette.text.r > 0.5,
                    self.options.backdrop,
                    self.options.backdrop_color.clone(),
                    self.options.material_options,
                ),
                open_ms: 180,
            },
        ));
    }
    fn queue(&mut self, command: Command) {
        self.actions.push(command);
        let _ = unsafe { PostMessageW(Some(self.owner), ACTION, WPARAM(self.hwnd.0 as usize), LPARAM(0)) };
    }
    fn invoke(&mut self, source: HWND, index: usize, action: Action, rect: RECT) {
        let Some(shown) = self.shown.get(index) else { return };
        let key = shown.icon.key();
        let action = if matches!(action, Action::Click) {
            if self.last_click.is_some_and(|(k, at)| {
                k == key && at.elapsed() < Duration::from_millis(unsafe { GetDoubleClickTime() }.into())
            }) {
                self.last_click = None;
                Action::DoubleClick
            } else {
                self.last_click = Some((key, Instant::now()));
                action
            }
        } else {
            action
        };
        self.data.tray_anchor.set(self.hwnd);
        self.publish();
        // Both mouse buttons can open a native popup over the bar or overflow.
        let _ = unsafe { SetForegroundWindow(source) };
        if self.tray_menu.is_none() {
            self.set_tray_order(HWND_NOTOPMOST);
        }
        match tray_host::invoke(
            &self.shown[index].icon,
            action,
            (rect.left + rect.right) / 2,
            rect.bottom,
        ) {
            Ok(thread) => self.tray_menu = Some((thread, Instant::now())),
            Err(error) => {
                if self.tray_menu.is_none() {
                    self.set_tray_order(HWND_TOPMOST);
                }
                eprintln!("Top bar notification icon: {error}");
            }
        }
    }
    fn pointer_target(&self, hit: Hit) -> Option<PointerTarget> {
        let tray_key = if let Hit::Tray(slot) = hit {
            let &index = self.inline.get(slot)?;
            Some(self.shown.get(index)?.icon.key())
        } else {
            None
        };
        Some(PointerTarget { hit, tray_key })
    }
    fn flyout_slot(&self, lp: LPARAM) -> Option<usize> {
        let scale = self.scale() * model::surface_visual(self.flyout_progress.value).scale;
        let x = (lp.0 as i16) as f32 / scale;
        let y = ((lp.0 >> 16) as i16) as f32 / scale;
        logic::flyout_hit(x, y, self.overflow.len())
    }
    fn tray_target(&self, hwnd: HWND, lp: LPARAM) -> Option<(usize, RECT)> {
        if hwnd == self.flyout {
            let slot = self.flyout_slot(lp)?;
            Some((*self.overflow.get(slot)?, self.flyout_rect(slot)))
        } else {
            let hit @ Hit::Tray(slot) = self.hit(hwnd, lp)? else {
                return None;
            };
            Some((*self.inline.get(slot)?, self.screen_rect(hit)))
        }
    }
    fn click(&mut self, hit: Hit) {
        match hit {
            Hit::Tray(i) => {
                if let Some(&index) = self.inline.get(i) {
                    self.invoke(self.hwnd, index, Action::Click, self.screen_rect(hit));
                }
            }
            Hit::Overflow => self.toggle_flyout(),
            Hit::Wifi => self.open_panel(hit, sound::Entry::Wifi),
            Hit::Sound => self.open_panel(hit, sound::Entry::Sound),
            Hit::Controls => self.open_panel(hit, sound::Entry::Home),
            Hit::Previous | Hit::PlayPause | Hit::Next => {
                if let Some(track) = &self.track {
                    let control = match hit {
                        Hit::Previous => Control::Previous,
                        Hit::Next => Control::Next,
                        _ => Control::PlayPause,
                    };
                    self.queue(Command::Media(Control::for_track(&track.media, control)));
                }
            }
            Hit::Clock => {
                self.open_panel(Hit::Clock, sound::Entry::Calendar);
            }
            Hit::Battery => {
                let _ = unsafe {
                    PostMessageW(
                        Some(self.owner),
                        BATTERY_SETTINGS,
                        WPARAM(self.hwnd.0 as usize),
                        LPARAM(0),
                    )
                };
            }
        }
    }
    fn hit(&self, hwnd: HWND, lp: LPARAM) -> Option<Hit> {
        let (x, y) = (
            (lp.0 as i16) as f32 / self.scale(),
            ((lp.0 >> 16) as i16) as f32 / self.scale(),
        );
        if hwnd == self.island {
            if self.music()
                && y >= 0.
                && y < self.options.height as f32
                && let Some(hit) =
                    model::IslandLayout::new(self.island_width(), self.compact, self.time_width).media_hit(x)
            {
                return Some(hit);
            }
            Some(Hit::Clock)
        } else {
            self.layout().hit(x, y)
        }
    }
    fn begin_drag(&mut self, _hwnd: HWND, index: usize) {
        if unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } & i16::MIN == 0 {
            return;
        }
        if let Some(icon) = self.shown.get(index) {
            let mut p = POINT::default();
            let _ = unsafe { GetCursorPos(&mut p) };
            self.drag = Some(TrayDrag {
                key: icon.icon.key(),
                gesture: model::DragGesture::new((p.x, p.y)),
            });
            self.suppress_tray_release = true;
            self.tray_right_press = None;
            self.flyout_press = None;
            self.down = None;
            unsafe {
                let _ = GetAsyncKeyState(VK_ESCAPE.0 as i32);
                SetTimer(Some(self.hwnd), DRAG, 15, None);
            }
        }
    }
    fn cancel_drag(&mut self) {
        self.drag = None;
        self.drag_target = None;
        self.down = None;
        self.tray_right_press = None;
        self.flyout_press = None;
        self.press_level.target = 0.;
        let _ = unsafe { KillTimer(Some(self.hwnd), DRAG) };
        self.invalidate();
        let _ = unsafe { InvalidateRect(Some(self.flyout), None, false) };
    }
    fn drop_target_at(&self, p: POINT) -> Option<bool> {
        let x = (p.x - self.rect.left) as f32 / self.scale();
        if p.y >= self.rect.top
            && p.y < self.rect.bottom
            && (8. ..(44. + self.inline.len() as f32 * 28.).max(128.)).contains(&x)
        {
            return Some(true);
        }
        let mut r = RECT::default();
        let _ = unsafe { GetWindowRect(self.flyout, &mut r) };
        (self.flyout_open && p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom).then_some(false)
    }
    fn poll_drag(&mut self) {
        if self.drag.is_none() {
            return;
        }
        let mut p = POINT::default();
        if unsafe { GetCursorPos(&mut p) }.is_err() {
            self.cancel_drag();
            return;
        }
        let target = self.drop_target_at(p);
        let left = unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } & i16::MIN != 0;
        let ctrl = unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } & i16::MIN != 0;
        let escape = unsafe { GetAsyncKeyState(VK_ESCAPE.0 as i32) } != 0;
        let drag = self.drag.as_mut().unwrap();
        let step = drag.gesture.sample((p.x, p.y), left, ctrl, escape, target);
        let moved = drag.gesture.moved;
        if moved && !self.flyout_open {
            self.show_flyout(false);
        }
        if self.drag_target != target {
            self.drag_target = target;
            self.invalidate();
            let _ = unsafe { InvalidateRect(Some(self.flyout), None, false) };
        }
        match step {
            model::DragStep::Moving => {}
            model::DragStep::Commit(promoted) => {
                let key = self.drag.as_ref().unwrap().key;
                let result = self
                    .shown
                    .iter()
                    .find(|s| s.icon.key() == key)
                    .map(|icon| self.preferences.save(&icon.icon, promoted));
                self.cancel_drag();
                match result {
                    Some(Ok(())) => {
                        self.partition();
                        self.invalidate();
                        post_to_bars();
                    }
                    Some(Err(error)) => eprintln!("Top bar tray preference: {error}"),
                    None => {}
                }
            }
            model::DragStep::Cancel | model::DragStep::Finished => self.cancel_drag(),
        }
    }
}

fn track_metadata_changed(old: Option<&NotchTrack>, new: Option<&NotchTrack>) -> bool {
    old.zip(new).is_some_and(|(old, new)| {
        old.media.source != new.media.source
            || old.media.title != new.media.title
            || old.media.artist != new.media.artist
    })
}
fn dispatch_release<T: PartialEq>(pressed: Option<T>, released: Option<T>, dispatch: impl FnOnce(T)) {
    if pressed == released
        && let Some(hit) = released
    {
        dispatch(hit);
    }
}
/// Presentation succeeds before any native visibility/focus transition.
fn present_then_show<E>(prepared: std::result::Result<(), E>, show: impl FnOnce()) -> std::result::Result<(), E> {
    prepared?;
    show();
    Ok(())
}

thread_local! { static BARS:RefCell<Vec<HWND>>=const{RefCell::new(Vec::new())}; }
fn post_to_bars() {
    BARS.with_borrow(|bars| {
        for &hwnd in bars {
            let _ = unsafe { PostMessageW(Some(hwnd), SYNC, WPARAM(0), LPARAM(0)) };
        }
    });
}

fn hide_material_host_before_message(message: u32, timer: usize, geometry_motion: bool) -> bool {
    let _ = (timer, geometry_motion);
    matches!(
        message,
        WM_DPICHANGED
            | WM_DISPLAYCHANGE
            | WM_SETTINGCHANGE
            | WM_SHOWWINDOW
            | WM_WINDOWPOSCHANGING
            | WM_DESTROY
            | WM_NCDESTROY
            | MATERIAL
    )
}
fn hide_material_hosts_before_message(
    message: u32,
    timer: usize,
    island_motion: bool,
    popup_motion: bool,
) -> (bool, bool) {
    (
        hide_material_host_before_message(message, timer, island_motion),
        hide_material_host_before_message(message, timer, popup_motion),
    )
}
#[cfg(test)]
mod material_message_tests {
    use super::*;
    #[test]
    fn static_polling_and_content_fades_do_not_hide_companions() {
        assert!(!hide_material_host_before_message(WM_TIMER, TICK, false));
        assert!(!hide_material_host_before_message(WM_TIMER, MOTION, false));
        assert!(!hide_material_host_before_message(WM_MOUSEMOVE, 0, false));
        assert!(!hide_material_host_before_message(WM_TIMER, MOTION, true));
        assert!(hide_material_host_before_message(WM_SETTINGCHANGE, 0, false));
        assert!(hide_material_host_before_message(WM_DESTROY, 0, false));
    }
    #[test]
    fn menu_motion_keeps_static_island_glass_and_island_motion_keeps_static_menu_glass() {
        assert_eq!(
            hide_material_hosts_before_message(WM_TIMER, MOTION, false, true),
            (false, false)
        );
        assert_eq!(
            hide_material_hosts_before_message(WM_TIMER, MOTION, true, false),
            (false, false)
        );
        assert_eq!(
            hide_material_hosts_before_message(WM_TIMER, MOTION, false, false),
            (false, false)
        );
        for message in [WM_DISPLAYCHANGE, WM_DPICHANGED, WM_SETTINGCHANGE, WM_DESTROY, MATERIAL] {
            assert_eq!(
                hide_material_hosts_before_message(message, 0, false, true),
                (true, true)
            );
        }
    }
}

/// Keep companion native effects outside the foreground state borrow. Taking
/// hosts also prevents a nested foreground callback from touching live targets.
unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<BarState>;
        if pointer.is_null() {
            return proc_inner(hwnd, msg, wp, lp);
        }
        Rc::increment_strong_count(pointer);
        let state = Rc::from_raw(pointer);
        // TrackPopupMenu runs a modal loop. Leave the material hosts in state
        // so nested window messages can still draw and retire their surfaces.
        let chevron_menu = if msg == WM_RBUTTONUP {
            state.try_borrow_mut().ok().and_then(|mut s| {
                if s.visible
                    && !s.native_menu_open
                    && s.drag.is_none()
                    && s.hwnd == hwnd
                    && s.tray_right_press == Some(TrayRightPress::Chevron)
                    && s.hit(hwnd, lp) == Some(Hit::Overflow)
                {
                    s.tray_right_press = None;
                    s.native_menu_open = true;
                    s.dismiss_flyout();
                    Some(s.owner)
                } else {
                    None
                }
            })
        } else {
            None
        };
        if let Some(owner) = chevron_menu {
            super::run_topbar_chevron_menu(hwnd, owner, || {
                if GetWindowLongPtrW(hwnd, GWLP_USERDATA) != pointer as isize {
                    return false;
                }
                let mut s = state.borrow_mut();
                s.native_menu_open = false;
                s.visible
            });
            return LRESULT(0);
        }
        let snapshot = state
            .try_borrow_mut()
            .ok()
            .map(|mut s| (s.material_version, s.blur_host.take(), s.popup_material.take()));
        let Some((version, mut island_host, mut popup)) = snapshot else {
            return proc_inner(hwnd, msg, wp, lp);
        };
        let mut island_presentation = state.borrow().blur_presented;
        let (island_motion, popup_motion) = {
            let s = state.borrow();
            let island_motion = [&s.width, &s.reveal]
                .iter()
                .any(|spring| spring.value != spring.target || spring.velocity != 0.);
            let popup_motion = s.flyout_progress.active();
            (island_motion, popup_motion)
        };
        let (hide_island, hide_popup) = hide_material_hosts_before_message(msg, wp.0, island_motion, popup_motion);
        if hide_island {
            island_presentation = None;
            if let Some(host) = &island_host {
                host.hide();
            }
        }
        if hide_popup && let Some(host) = &popup {
            host.hide();
        }
        let result = proc_inner(hwnd, msg, wp, lp);
        if msg == WM_NCDESTROY
            || !crate::theme::material_is_current(
                version,
                state.borrow().material_version,
                true,
                GetWindowLongPtrW(hwnd, GWLP_USERDATA) == pointer as isize,
            )
        {
            return result;
        }
        if msg == MATERIAL {
            island_presentation = None;
            drop(island_host.take());
            drop(popup.take());
            let (owner, island_blur, foreground, dark, backdrop, color, options) = {
                let s = state.borrow();
                (
                    // The top bar owns the foreground too. Launcher activation
                    // must not reorder the glass as a separate owner group.
                    s.hwnd,
                    s.options.island_blur,
                    s.flyout,
                    s.popup_palette.text.r > 0.5,
                    s.options.backdrop,
                    s.options.backdrop_color.clone(),
                    s.options.material_options,
                )
            };
            if island_blur && crate::theme::island_blur_available() {
                match super::topbar_blur::BlurHost::new(Some(owner)) {
                    Ok(host) => island_host = Some(host),
                    Err(error) => {
                        crate::logging::event("island.material.prepare.failed", format_args!("error={error}"))
                    }
                }
            }
            if !crate::theme::material_is_current(
                version,
                state.borrow().material_version,
                true,
                GetWindowLongPtrW(hwnd, GWLP_USERDATA) == pointer as isize
                    && GetWindowLongPtrW(foreground, GWLP_USERDATA) == pointer as isize,
            ) {
                return result;
            }
            popup = Some(crate::theme::SurfaceMaterial::prepare(
                Some(owner),
                foreground,
                dark,
                backdrop,
                &color,
                options,
            ));
            if GetWindowLongPtrW(hwnd, GWLP_USERDATA) != pointer as isize
                || GetWindowLongPtrW(foreground, GWLP_USERDATA) != pointer as isize
            {
                return result;
            }
            let mut s = state.borrow_mut();
            if s.material_version != version {
                return result;
            }
            if let Some(material) = &popup {
                s.popup_palette = material.palette.clone();
            }
            s.popup_prepared = false;
            s.island_prepared = false;
            s.invalidate();
            let _ = InvalidateRect(Some(foreground), None, false);
            let panel = s.panel_identity.map(|id| PanelTarget {
                id,
                epoch: s.panel_epoch,
                material_version: s.material_version,
            });
            drop(s);
            if let Some(target) = panel.filter(|target| bar_panel_current(&state, *target)) {
                sound::apply_theme(target.id.hwnd, dark, backdrop, &color, options);
            }
        }
        let (current_version, island, flyout, dpi, island_ready, popup_ready, reveal, opacity, scale) = {
            let s = state.borrow();
            (
                s.material_version,
                s.island,
                s.flyout,
                s.dpi,
                s.visible && s.island_prepared,
                s.flyout_open && s.popup_prepared,
                s.island_painted_reveal,
                s.flyout_progress.value,
                model::surface_visual(s.flyout_progress.value).scale,
            )
        };
        if !crate::theme::material_is_current(
            version,
            current_version,
            true,
            GetWindowLongPtrW(hwnd, GWLP_USERDATA) == pointer as isize
                && GetWindowLongPtrW(island, GWLP_USERDATA) == pointer as isize
                && GetWindowLongPtrW(flyout, GWLP_USERDATA) == pointer as isize,
        ) {
            return result;
        }
        if let Some(host) = &mut island_host {
            let mut bounds = RECT::default();
            if island_ready && IsWindowVisible(island).as_bool() && GetWindowRect(island, &mut bounds).is_ok() {
                let radius = 7_f32.min((bounds.bottom - bounds.top) as f32 * 96. / dpi as f32 * 0.3);
                let presentation = (
                    (
                        bounds.left,
                        bounds.top,
                        bounds.right - bounds.left,
                        bounds.bottom - bounds.top,
                        dpi,
                    ),
                    (reveal.clamp(0., 1.) * 255.).round() as u8,
                );
                if island_presentation != Some(presentation) || !host.placement_current(island) {
                    if let Err(error) = host.sync(island, bounds, dpi, radius, reveal) {
                        crate::logging::event("island.material.present.failed", format_args!("error={error}"));
                        island_host.take();
                        island_presentation = None;
                    } else {
                        island_presentation = Some(presentation);
                    }
                }
            } else {
                host.hide();
                island_presentation = None;
            }
        }
        if !crate::theme::material_is_current(
            version,
            state.borrow().material_version,
            true,
            GetWindowLongPtrW(hwnd, GWLP_USERDATA) == pointer as isize
                && GetWindowLongPtrW(flyout, GWLP_USERDATA) == pointer as isize
                && GetWindowLongPtrW(island, GWLP_USERDATA) == pointer as isize,
        ) {
            return result;
        }
        let mut repaint = false;
        if let Some(material) = &mut popup {
            let mut bounds = RECT::default();
            if popup_ready && IsWindowVisible(flyout).as_bool() && GetWindowRect(flyout, &mut bounds).is_ok() {
                repaint = !material.present(bounds, dpi, render::TOPBAR_FLYOUT_RADIUS * scale, opacity);
            } else {
                material.hide();
            }
        }
        if GetWindowLongPtrW(hwnd, GWLP_USERDATA) != pointer as isize
            || GetWindowLongPtrW(flyout, GWLP_USERDATA) != pointer as isize
            || GetWindowLongPtrW(island, GWLP_USERDATA) != pointer as isize
        {
            return result;
        }
        let mut s = state.borrow_mut();
        if s.material_version == version {
            if let Some(material) = &popup {
                s.popup_palette = material.palette.clone();
            }
            s.blur_host = island_host;
            s.blur_presented = island_presentation;
            s.popup_material = popup;
            if repaint {
                s.popup_prepared = false;
                let _ = InvalidateRect(Some(flyout), None, false);
            }
        }
        result
    }
}

unsafe extern "system" fn proc_inner(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let c = &*(lp.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, c.lpCreateParams as isize);
        }
        if msg == WM_NCDESTROY {
            BARS.with_borrow_mut(|bars| bars.retain(|h| *h != hwnd));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        }
        if msg == WM_ERASEBKGND {
            return LRESULT(1);
        }
        let cell = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<BarState>;
        if cell.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        Rc::increment_strong_count(cell);
        let cell = Rc::from_raw(cell);
        let Ok(mut s) = cell.try_borrow_mut() else {
            return DefWindowProcW(hwnd, msg, wp, lp);
        };
        if msg == WM_MOUSEACTIVATE && hwnd == s.hwnd {
            return LRESULT(MA_NOACTIVATE as isize);
        }
        if hwnd == s.flyout
            && s.flyout_closing
            && matches!(msg, WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP)
        {
            return LRESULT(0);
        }
        match msg {
            WM_CREATE => {}
            WM_TIMER if wp.0 == TICK => s.poll(),
            WM_TIMER if wp.0 == MOTION => s.frame(),
            WM_TIMER if wp.0 == DRAG => s.poll_drag(),
            WM_TIMER if wp.0 == TITLE_TIMEOUT => s.media_state(),
            SYNC => {
                s.preferences = Preferences::load();
                s.partition();
                s.invalidate();
            }
            WM_DPICHANGED | WM_DISPLAYCHANGE => {
                s.cancel_drag();
                s.close_panel();
                s.close_flyout();
                s.place();
                s.measure_content();
                s.media_state();
            }
            WM_SETTINGCHANGE => {
                s.animations_enabled = client_animations_enabled();
                s.material();
                s.measure_content();
                s.media_state();
            }
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut paint);
                if hwnd == s.island {
                    if let Err(error) = s.present_island() {
                        eprintln!("Top bar island draw: {error}");
                    } else {
                        s.animate();
                    }
                    let _ = EndPaint(hwnd, &paint);
                    return LRESULT(0);
                }
                let layout = s.layout();
                let is_bar = hwnd == s.hwnd;
                let BarState {
                    renderer,
                    flyout_renderer,
                    palette,
                    popup_palette,
                    status,
                    hover,
                    hover_level,
                    drag_target,
                    shown,
                    inline,
                    overflow,
                    flyout_hover,
                    flyout_progress,
                    commands,
                    ..
                } = &mut *s;
                let result = if is_bar {
                    renderer.draw_topbar(
                        hwnd,
                        render::TopbarFrame {
                            dpi: GetDpiForWindow(hwnd),
                            palette,
                            layout,
                            shown,
                            inline,
                            status,
                            commands,
                            hover: *hover,
                            hover_level: hover_level.value,
                            drag_target: *drag_target,
                        },
                    )
                } else {
                    flyout_renderer.draw_topbar_flyout(
                        hwnd,
                        render::TopbarFlyoutFrame {
                            dpi: GetDpiForWindow(hwnd),
                            palette: popup_palette,
                            shown,
                            overflow,
                            hover: *flyout_hover,
                            presentation: (
                                model::surface_visual(flyout_progress.value).scale,
                                flyout_progress.value.clamp(0., 1.),
                            ),
                        },
                    )
                };
                if !is_bar {
                    s.popup_prepared = result.is_ok();
                }
                if let Err(error) = result {
                    eprintln!("Top bar draw: {error}");
                }
                let _ = EndPaint(hwnd, &paint);
            }
            WM_MOUSEMOVE => {
                if s.drag.is_some() {
                    s.poll_drag();
                } else if hwnd == s.flyout {
                    s.flyout_hover = s.flyout_slot(lp);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                } else {
                    let hit = s.hit(hwnd, lp);
                    if hit != s.hover {
                        let previous = model::interaction_surfaces(s.hover);
                        let next = model::interaction_surfaces(hit);
                        s.hover = hit;
                        s.hover_level.value = 0.;
                        s.hover_level.target = if hit.is_some() { 1. } else { 0. };
                        s.animate();
                        s.invalidate_surfaces(previous.0 || next.0, previous.1 || next.1);
                    }
                }
                let _ = TrackMouseEvent(&mut TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                });
            }
            WM_MOUSELEAVE => {
                s.hover = None;
                s.hover_level.target = 0.;
                s.animate();
            }
            WM_LBUTTONDOWN => {
                s.suppress_tray_release = false;
                if hwnd == s.flyout {
                    if let Some(i) = s.flyout_slot(lp).and_then(|i| s.overflow.get(i).copied()) {
                        s.flyout_press = s.shown.get(i).map(|shown| shown.icon.key());
                        s.begin_drag(hwnd, i);
                    }
                } else {
                    s.down = s.hit(hwnd, lp).and_then(|hit| s.pointer_target(hit));
                    s.press_level.target = 1.;
                    s.animate();
                    if let Some(Hit::Tray(i)) = s.down.map(|target| target.hit)
                        && let Some(&index) = s.inline.get(i)
                    {
                        s.begin_drag(hwnd, index);
                    }
                }
            }
            WM_LBUTTONUP => {
                if s.drag.is_some() || s.suppress_tray_release {
                    s.poll_drag();
                    s.suppress_tray_release = false;
                    s.down = None;
                } else if hwnd == s.flyout {
                    if let Some(i) = s.flyout_slot(lp)
                        && let Some(&index) = s.overflow.get(i)
                        && s.flyout_press == s.shown.get(index).map(|shown| shown.icon.key())
                    {
                        let rect = s.flyout_rect(i);
                        s.invoke(hwnd, index, Action::Click, rect);
                    }
                    s.flyout_press = None;
                } else {
                    let released = s.hit(hwnd, lp).and_then(|hit| s.pointer_target(hit));
                    let pressed = s.down.take();
                    dispatch_release(pressed, released, |target| s.click(target.hit));
                }
                s.press_level.target = 0.;
                s.animate();
            }
            WM_CANCELMODE => {
                s.cancel_drag();
                s.flyout_press = None;
                s.down = None;
                s.press_level.target = 0.;
                s.animate();
            }
            WM_RBUTTONDOWN => {
                s.tray_right_press = if hwnd == s.hwnd && s.hit(hwnd, lp) == Some(Hit::Overflow) {
                    Some(TrayRightPress::Chevron)
                } else {
                    s.tray_target(hwnd, lp)
                        .and_then(|(index, _)| s.shown.get(index))
                        .map(|shown| TrayRightPress::Icon(hwnd, shown.icon.key()))
                };
            }
            WM_RBUTTONUP => {
                let pressed = s.tray_right_press.take();
                if s.drag.is_none()
                    && let Some((index, rect)) = s.tray_target(hwnd, lp)
                    && let Some(shown) = s.shown.get(index)
                    && pressed == Some(TrayRightPress::Icon(hwnd, shown.icon.key()))
                {
                    s.invoke(hwnd, index, Action::Menu, rect);
                }
            }
            WM_MOUSEWHEEL => {
                let mut point = POINT {
                    x: (lp.0 as i16) as i32,
                    y: ((lp.0 >> 16) as i16) as i32,
                };
                let _ = ScreenToClient(hwnd, &mut point);
                let point = LPARAM(((point.y as u16 as u32) << 16 | point.x as u16 as u32) as isize);
                if s.hit(hwnd, point) == Some(Hit::Sound) {
                    let delta = ((wp.0 >> 16) as i16) as i32;
                    if s.actions.volume_step(delta) {
                        let _ = PostMessageW(Some(s.owner), ACTION, WPARAM(s.hwnd.0 as usize), LPARAM(0));
                    }
                }
            }
            WM_KEYDOWN => {
                if wp.0 == VK_ESCAPE.0 as usize {
                    s.cancel_drag();
                    s.dismiss_flyout();
                    s.down = None;
                    let _ = ReleaseCapture();
                } else if hwnd == s.island {
                    let hit = if wp.0 == VK_LEFT.0 as usize {
                        Some(Hit::Previous)
                    } else if wp.0 == VK_RIGHT.0 as usize {
                        Some(Hit::Next)
                    } else if wp.0 == VK_SPACE.0 as usize {
                        Some(Hit::PlayPause)
                    } else if wp.0 == VK_RETURN.0 as usize {
                        Some(Hit::Clock)
                    } else {
                        None
                    };
                    if let Some(hit) = hit
                        && (s.music() || hit == Hit::Clock)
                    {
                        s.focused = Some(hit);
                        s.click(hit);
                    }
                }
            }
            WM_ACTIVATE
                if wp.0 & 0xFFFF == WA_INACTIVE as usize
                    && hwnd == s.flyout
                    && s.drag.is_none()
                    && s.tray_menu.is_none() =>
            {
                s.dismiss_flyout()
            }
            _ => return DefWindowProcW(hwnd, msg, wp, lp),
        }
        let pending = s.pending_panel.take();
        drop(s);
        if let Some((epoch, request)) = pending {
            match request.open() {
                Ok(panel) => {
                    let id = sound::identity(panel);
                    let mut state = cell.borrow_mut();
                    if state.panel_epoch == epoch && state.visible && IsWindow(Some(state.hwnd)).as_bool() {
                        state.panel = panel;
                        state.panel_identity = id;
                        state.sync_panel_opener();
                    } else if let Some(id) = id {
                        drop(state);
                        sound::destroy(id);
                    }
                }
                Err(error) => crate::logging::event("controls.open.failed", format_args!("owner=topbar error={error}")),
            }
        }
        LRESULT(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_material_reload_changes_menu_generation_options_without_changing_island_preferences() {
        let mut config = crate::config::Config {
            backdrop: Backdrop::Transparent,
            ..Default::default()
        };
        let old = Options::from_config(&config);
        config.backdrop_transparency = Some(100);
        config.backdrop_blur = Some(true);
        let new = Options::from_config(&config);
        assert!(
            old != new,
            "configure must invalidate retained material and cached foreground"
        );
        assert_eq!(new.material_options, config.material_options());
        assert_eq!(
            (old.opacity, old.island_opacity, old.island_blur),
            (new.opacity, new.island_opacity, new.island_blur)
        );
    }

    #[test]
    fn rapid_media_actions_are_delivered_in_order() {
        let mut actions = ActionQueue::default();
        for control in [Control::Previous, Control::PlayPause, Control::Next] {
            actions.push(Command::Media(control));
        }
        assert!(matches!(actions.pop(), Some(Command::Media(Control::Previous))));
        assert!(matches!(actions.pop(), Some(Command::Media(Control::PlayPause))));
        assert!(matches!(actions.pop(), Some(Command::Media(Control::Next))));
        assert!(actions.pop().is_none());
    }
    #[test]
    fn rapid_volume_steps_accumulate_before_a_worker_reply() {
        let mut actions = ActionQueue {
            volume: Some(50),
            ..Default::default()
        };
        assert!(actions.volume_step(120));
        assert!(actions.volume_step(120));
        assert!(matches!(actions.pop(), Some(Command::Volume(52))));
        assert!(matches!(actions.pop(), Some(Command::Volume(54))));
        assert!(!actions.volume_step(20));
        assert!(actions.volume_step(40));
        assert!(matches!(actions.pop(), Some(Command::Volume(55))));
    }
    #[test]
    fn tray_preferences_round_trip_and_replace_previous_choices() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("topbar-prefs-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tray.toml");
        let mut prefs = Preferences::default();
        prefs.icons.insert("stable-app:1".into(), true);
        prefs.order.push("stable-app:1".into());
        prefs.write_to(&path).unwrap();
        prefs.icons.insert("stable-app:1".into(), false);
        prefs.order.clear();
        prefs.write_to(&path).unwrap();
        let saved: Preferences = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.icons.get("stable-app:1"), Some(&false));
        assert!(saved.order.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn runtime_settings_clamp_untrusted_numeric_values() {
        let config = crate::config::Config {
            topbar_height: 0,
            topbar_opacity: u32::MAX,
            topbar_island_opacity: 999,
            ..Default::default()
        };
        let options = Options::from_config(&config);
        assert_eq!(
            (options.height, options.opacity, options.island_opacity),
            (24, 100, 100)
        );
    }
    #[test]
    fn stale_snapshot_does_not_reset_pending_wheel_steps() {
        let mut actions = ActionQueue {
            volume: Some(50),
            ..Default::default()
        };
        actions.volume_step(120);
        actions.volume_step(120);
        assert!(matches!(actions.pop(), Some(Command::Volume(52))));
        actions.sync_volume(Some(52));
        actions.volume_step(120);
        assert!(matches!(actions.pop(), Some(Command::Volume(54))));
        assert!(matches!(actions.pop(), Some(Command::Volume(56))));
        actions.sync_volume(Some(56));
        assert_eq!(actions.volume, Some(56));
    }
}

#[cfg(test)]
mod overflow_tests {
    use super::*;
    #[test]
    fn steady_player_polls_and_play_pause_do_not_restart_title_transition() {
        let old = NotchTrack {
            media: crate::commands::media::State {
                source: "music".into(),
                title: "Track A".into(),
                artist: "Artist".into(),
                app: "Music".into(),
                playing: true,
                timeline: None,
            },
            artwork: None,
        };
        let mut next = old.clone();
        assert!(!track_metadata_changed(Some(&old), Some(&next)));
        next.media.playing = false;
        assert!(!track_metadata_changed(Some(&old), Some(&next)));
        next.media.artist = "Different artist".into();
        assert!(track_metadata_changed(Some(&old), Some(&next)));
        next.media.artist.clone_from(&old.media.artist);
        next.media.title = "Track B".into();
        assert!(track_metadata_changed(Some(&old), Some(&next)));
    }
    #[test]
    fn chevron_press_release_dispatches_one_open_and_next_click_closes() {
        let layout = Layout {
            width: 1280.,
            height: 28.,
            inline: 3,
            battery: true,
        };
        let mut down = layout.hit(12. + 3. * 28. + 14., 14.);
        let mut open = false;
        let mut paints = 0;
        let mut shows = 0;
        let mut click = |hit| {
            assert_eq!(hit, Hit::Overflow);
            if open {
                open = false
            } else {
                paints += 1;
                present_then_show(Ok::<(), ()>(()), || {
                    open = true;
                    shows += 1;
                })
                .unwrap();
            }
        };
        dispatch_release(down.take(), layout.hit(110., 14.), &mut click);
        dispatch_release(down.take(), layout.hit(110., 14.), &mut click);
        down = layout.hit(110., 14.);
        dispatch_release(down.take(), layout.hit(110., 14.), &mut click);
        // Cancelled/customizing press and release outside the target do not dispatch.
        dispatch_release(None, layout.hit(110., 14.), &mut click);
        dispatch_release(layout.hit(110., 14.), None, &mut click);
        assert!(!open);
        assert_eq!((paints, shows), (1, 1));
    }
    #[test]
    fn inline_tray_release_rejects_replacement_in_same_slot() {
        let pressed = PointerTarget {
            hit: Hit::Tray(0),
            tray_key: Some(tray_host::Key::Window { hwnd: 100, uid: 1 }),
        };
        let released = PointerTarget {
            tray_key: Some(tray_host::Key::Window { hwnd: 200, uid: 1 }),
            ..pressed
        };
        let mut dispatched = false;
        dispatch_release(Some(pressed), Some(released), |_| dispatched = true);
        assert!(!dispatched);
        dispatch_release(Some(pressed), Some(pressed), |_| dispatched = true);
        assert!(dispatched);
    }
    #[test]
    fn first_frame_precedes_visibility_and_failure_does_not_open() {
        let events = RefCell::new(Vec::new());
        let paint = || {
            events.borrow_mut().push("pixels");
            Ok::<(), ()>(())
        };
        present_then_show(paint(), || {
            events.borrow_mut().push("visible");
        })
        .unwrap();
        assert_eq!(*events.borrow(), ["pixels", "visible"]);
        assert!(
            present_then_show(Err::<(), _>("lost target"), || events
                .borrow_mut()
                .push("unexpected show"))
            .is_err()
        );
        assert_eq!(events.borrow().len(), 2);
    }
}
