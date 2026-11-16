//! Start menu pins, layout and bare Windows key handling.

use std::{cell::Cell, io};
use windows::Win32::{
    Foundation::*,
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

#[path = "start/app_menu.rs"]
pub(crate) mod app_menu;

pub const TOGGLE: u32 = WM_APP + 29;
pub const WIDTH: f32 = 544.;
pub const HEIGHT: f32 = 390.;
pub const RADIUS: f32 = 22.;
pub const FOOTER: f32 = 48.;
pub const CAPACITY: usize = 12;

pub use crate::animation::Motion;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppMenuAction {
    Open,
    Administrator,
    Location,
    SetPinned(bool),
    SetDockPinned(bool),
    CopyPath,
    AppSettings,
    Uninstall,
}
impl AppMenuAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Administrator => "Run as administrator",
            Self::Location => "Open file location",
            Self::SetPinned(true) => "Pin to Wisp Start",
            Self::SetPinned(false) => "Unpin from Wisp Start",
            Self::SetDockPinned(true) => "Pin to Wisp Dock",
            Self::SetDockPinned(false) => "Unpin from Wisp Dock",
            Self::CopyPath => "Copy path",
            Self::AppSettings => "App settings",
            Self::Uninstall => "Uninstall (Windows Settings)",
        }
    }
    pub fn code(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Administrator => "admin",
            Self::Location => "location",
            Self::SetPinned(true) => "pin",
            Self::SetPinned(false) => "unpin",
            Self::SetDockPinned(true) => "dock_pin",
            Self::SetDockPinned(false) => "dock_unpin",
            Self::CopyPath => "copy_path",
            Self::AppSettings => "app_settings",
            Self::Uninstall => "uninstall_settings",
        }
    }
}
pub fn app_menu_actions(pinned: bool, dock_pinned: bool) -> Vec<AppMenuAction> {
    vec![
        AppMenuAction::Open,
        AppMenuAction::Administrator,
        AppMenuAction::Location,
        AppMenuAction::SetPinned(!pinned),
        AppMenuAction::SetDockPinned(!dock_pinned),
        AppMenuAction::CopyPath,
        AppMenuAction::AppSettings,
        AppMenuAction::Uninstall,
    ]
}

/// Search action scopes retain the app kind, but only launch rows own an app
/// menu. Primary activation must also retain a scoped row's elevation intent.
pub fn result_app_action(result: &crate::search::ResultItem, force_admin: bool) -> Option<(&str, AppMenuAction)> {
    match &result.action {
        crate::search::Action::Launch { id, admin, .. } if result.kind == crate::search::Kind::App => Some((
            id,
            if *admin || force_admin {
                AppMenuAction::Administrator
            } else {
                AppMenuAction::Open
            },
        )),
        _ => None,
    }
}

pub fn disabled_reason(action: AppMenuAction, app: &crate::indexer::AppEntry) -> Option<&'static str> {
    match action {
        AppMenuAction::Administrator if app.id.contains('!') && !crate::indexer::absolute_desktop_path(&app.id) => {
            Some("Packaged apps cannot elevate")
        }
        AppMenuAction::Administrator
            if app
                .desktop
                .as_ref()
                .is_none_or(|desktop| !crate::indexer::executable_path(&desktop.target)) =>
        {
            Some("No desktop launch target")
        }
        AppMenuAction::Location | AppMenuAction::CopyPath
            if !app.path.as_deref().is_some_and(crate::indexer::absolute_desktop_path) =>
        {
            Some("No local file path")
        }
        _ => None,
    }
}

pub fn resolve_app_action<'a>(
    id: &str,
    apps: &'a [crate::indexer::AppEntry],
    actions: &[AppMenuAction],
    picked: Option<usize>,
) -> Option<(&'a crate::indexer::AppEntry, AppMenuAction)> {
    let app = apps.iter().find(|app| app.id == id)?;
    let action = *actions.get(picked?)?;
    if disabled_reason(action, app).is_some() {
        return None;
    }
    Some((app, action))
}

pub fn app_snapshot_matches(
    expected: &crate::indexer::AppEntry,
    current: &crate::indexer::AppEntry,
    expected_epoch: (u64, u64),
    current_epoch: (u64, u64),
) -> bool {
    expected_epoch == current_epoch && expected == current
}
/// A pressed inline verb belongs to one app and one capability, even when the
/// index or selection refreshes before the pointer is released.
pub fn inline_identity_matches(
    pressed_id: &str,
    pressed: AppMenuAction,
    selected_id: &str,
    selected: AppMenuAction,
) -> bool {
    pressed_id == selected_id && pressed == selected
}

const DEFAULT_APPS: &[&str] = &[
    "File Explorer",
    "Firefox",
    "Google Chrome",
    "Telegram",
    "Visual Studio Code",
    "Terminal",
    "Notepad",
    "Settings",
    "Calculator",
    "Photos",
    "Spotify",
    "Mail",
];

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Pins {
    ids: Option<Vec<String>>,
}

impl Pins {
    pub fn load() -> Self {
        std::fs::read_to_string(crate::util::data_dir().join("start-pins.toml"))
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn apps<'a>(&self, apps: &'a [crate::indexer::AppEntry]) -> Vec<&'a crate::indexer::AppEntry> {
        if let Some(ids) = &self.ids {
            ids.iter()
                .filter_map(|id| apps.iter().find(|app| &app.id == id))
                .take(CAPACITY)
                .collect()
        } else {
            DEFAULT_APPS
                .iter()
                .filter_map(|name| apps.iter().find(|app| app.name.eq_ignore_ascii_case(name)))
                .take(CAPACITY)
                .collect()
        }
    }

    pub fn set_pinned(&mut self, id: &str, pinned: bool, apps: &[crate::indexer::AppEntry]) -> io::Result<()> {
        self.set_pinned_to(id, pinned, apps, &crate::util::data_dir().join("start-pins.toml"))
    }

    fn set_pinned_to(
        &mut self,
        id: &str,
        pinned: bool,
        apps: &[crate::indexer::AppEntry],
        path: &std::path::Path,
    ) -> io::Result<()> {
        let mut ids = self
            .ids
            .clone()
            .unwrap_or_else(|| self.apps(apps).into_iter().map(|app| app.id.clone()).collect());
        let existing = ids.iter().position(|pin| pin == id);
        if existing.is_some() == pinned {
            return Ok(());
        }
        if let Some(index) = existing {
            ids.remove(index);
        } else if ids.len() < CAPACITY {
            ids.push(id.into());
        } else {
            return Err(io::Error::other("Unpin an app before adding another."));
        }
        let next = Self { ids: Some(ids) };
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, toml::to_string(&next).map_err(io::Error::other)?)?;
        std::fs::rename(tmp, path)?;
        *self = next;
        Ok(())
    }
}

pub const LIST_TOP: f32 = 76.;
pub const ACTIONS: f32 = 56.;
pub const INLINE_BASE: usize = CAPACITY + 3;

/// A fixed content budget keeps Home, search and the utility bar in place.
pub fn rows(height: f32) -> usize {
    ((height - FOOTER - ACTIONS - LIST_TOP) / crate::render::layout::ROW_H).max(0.) as usize
}
pub fn row_top(row: usize) -> f32 {
    LIST_TOP + row as f32 * crate::render::layout::ROW_H
}
pub fn row_at(y: f32, count: usize, height: f32) -> Option<usize> {
    if y < LIST_TOP || y >= height - FOOTER - ACTIONS {
        return None;
    }
    let row = ((y - LIST_TOP) / crate::render::layout::ROW_H) as usize;
    (row < count.min(rows(height))).then_some(row)
}

/// Nominal artwork size is independent of the square tile and keyboard order.
pub fn pin_icon_size(tile_height: f32) -> f32 {
    (tile_height - 32.).clamp(12., 44.).min((tile_height - 8.).max(0.)) * 0.6
}

const GRID_TOP: f32 = 84.;
const GRID_GAP: f32 = 4.;
const GRID_BOTTOM: f32 = 12.;

/// Home uses the same square cells at its actual monitor-constrained width.
/// Search and live surfaces retain their separate HEIGHT budget.
pub fn home_height(width: f32) -> f32 {
    let side = ((width - 32.).max(0.) / 6. - GRID_GAP).max(0.);
    (((GRID_TOP + 2. * side + GRID_GAP + GRID_BOTTOM + FOOTER) / 4.).ceil() * 4.).min(HEIGHT)
}

pub fn tile(index: usize, width: f32, height: f32) -> (f32, f32, f32, f32) {
    let pitch = (width - 32.).max(0.) / 6.;
    let available = ((height - FOOTER - GRID_TOP - GRID_BOTTOM - GRID_GAP) / 2.).max(0.);
    let side = (pitch - GRID_GAP).max(0.).min(available);
    let x = 16. + (index % 6) as f32 * pitch + (pitch - side) / 2.;
    let y = GRID_TOP + (index / 6) as f32 * (side + GRID_GAP);
    (x, y, x + side, y + side)
}

pub fn footer(index: usize, width: f32, height: f32) -> (f32, f32, f32, f32) {
    let margin = 16_f32.min(width.max(0.) / 2.);
    let available = (width - 2. * margin).max(0.);
    let gap = 8_f32.min(available / 2.);
    let footer_height = FOOTER.min(height.max(0.));
    let side = 36_f32
        .min(((available - 2. * gap) / 3.).max(0.))
        .min((footer_height - 12.).max(0.));
    let left = match index {
        0 => margin,
        1 => width - margin - 2. * side - gap,
        _ => width - margin - side,
    };
    let top = height - footer_height + (footer_height - side) / 2.;
    (left, top, left + side, top + side)
}

/// Paint, pointer activation and keyboard context anchors share the same bounds.
pub fn hit(x: f32, y: f32, width: f32, height: f32, pins: usize) -> Option<usize> {
    let inside = |(l, t, r, b)| x >= l && x < r && y >= t && y < b;
    (0..pins.min(CAPACITY))
        .find(|&i| inside(tile(i, width, height)))
        .or_else(|| (0..3).find(|&i| inside(footer(i, width, height))).map(|i| CAPACITY + i))
}

/// Tab/F6 traversal returns to query focus at either end; search keeps its
/// existing Tab drill-down while the query owns focus.
pub fn utility_focus(current: Option<usize>, actions: usize, backwards: bool) -> Option<usize> {
    let end = INLINE_BASE + actions;
    match current.filter(|i| (CAPACITY..end).contains(i)) {
        Some(i) if backwards => (i > CAPACITY).then(|| i - 1),
        Some(i) => (i + 1 < end).then_some(i + 1),
        None if backwards => Some(end - 1),
        None => Some(CAPACITY),
    }
}

pub fn inline_rect(index: usize, width: f32, height: f32) -> (f32, f32, f32, f32) {
    // Two rows preserve all three actions at narrow widths with full labels.
    let top = height - FOOTER - ACTIONS;
    if width < 440. {
        match index {
            0 => (16., top, width - 16., top + 24.),
            1 => (16., top + 24., width * 0.52, top + 52.),
            _ => (width * 0.52, top + 24., width - 16., top + 52.),
        }
    } else {
        let (l, r) = match index {
            0 => (16., width * 0.2),
            1 => (width * 0.2, width * 0.6),
            _ => (width * 0.6, width - 16.),
        };
        (l, top + 8., r, top + 44.)
    }
}

/// Start's position is independent of the compact launcher's setting. Physical
/// bounds are clamped before converting to DIPs, including negative monitors.
pub fn placement(work: RECT, dock_top: Option<i32>, dpi: u32, position: crate::config::Position) -> RECT {
    placement_height(work, dock_top, dpi, position, HEIGHT)
}

pub fn search_height(result_count: usize, max_results: usize, home_or_surface: bool) -> f32 {
    if home_or_surface {
        HEIGHT
    } else {
        (LIST_TOP + FOOTER + ACTIONS + 4. + result_count.min(max_results).max(1) as f32 * crate::render::layout::ROW_H)
            .min(HEIGHT)
    }
}

/// Keyboard context menus anchor inside the selected row, without a pointer
/// button or reserved ellipsis hit area.
pub fn result_menu_anchor(row: usize, width: f32) -> (f32, f32) {
    (width - 32., row_top(row) + crate::render::layout::ROW_H / 2.)
}

pub fn placement_height(
    work: RECT,
    dock_top: Option<i32>,
    dpi: u32,
    position: crate::config::Position,
    height: f32,
) -> RECT {
    let scale = dpi.max(1) as f32 / 96.;
    let work_w = (work.right - work.left).max(1);
    let work_h = (work.bottom - work.top).max(1);
    let margin = (16. * scale).round() as i32;
    let bottom = if position == crate::config::Position::Bottom {
        dock_top
            .unwrap_or(work.bottom)
            .min(work.bottom)
            .saturating_sub((8. * scale).round() as i32)
            .max(work.top + 1)
    } else {
        work.bottom
    };
    let available_h = (bottom - work.top).max(1);
    let w = ((WIDTH * scale).round() as i32).min((work_w - 2 * margin).max(1));
    let h = ((height * scale).round() as i32).min((available_h - margin).max(1));
    let left = work.left + (work_w - w) / 2;
    let top = if position == crate::config::Position::Center {
        work.top + (work_h - h) / 2
    } else {
        bottom - h
    };
    RECT {
        left,
        top,
        right: left + w,
        bottom: top + h,
    }
}

use crate::commands::power::Power;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FooterMenu {
    Profile,
    Power,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FooterAction {
    Power(Power),
    Quit,
    Remote(crate::remote_mode::Mode),
}
impl FooterAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Power(power) => power.title(),
            Self::Quit => "Quit Wisp",
            Self::Remote(crate::remote_mode::Mode::Auto) => "Remote: Auto",
            Self::Remote(crate::remote_mode::Mode::On) => "Remote: On",
            Self::Remote(crate::remote_mode::Mode::Off) => "Remote: Off",
        }
    }
}
impl FooterMenu {
    pub fn actions(self) -> &'static [FooterAction] {
        match self {
            Self::Profile => &[
                FooterAction::Power(Power::Lock),
                FooterAction::Power(Power::SignOut),
                FooterAction::Remote(crate::remote_mode::Mode::Auto),
                FooterAction::Remote(crate::remote_mode::Mode::On),
                FooterAction::Remote(crate::remote_mode::Mode::Off),
            ],
            Self::Power => &[
                FooterAction::Quit,
                FooterAction::Power(Power::Sleep),
                FooterAction::Power(Power::Restart),
                FooterAction::Power(Power::ShutDown),
            ],
        }
    }

    pub fn row(self, index: usize, width: f32, height: f32) -> (f32, f32, f32, f32) {
        let left = if self == Self::Profile { 18. } else { width - 188. };
        let row_height = 31_f32.min((height - FOOTER - 24.) / self.actions().len() as f32);
        let top = height - FOOTER - 14. - (self.actions().len() - index) as f32 * row_height;
        (left, top, left + 170., top + row_height)
    }
}

thread_local! {
    static OWNER: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
    static CANDIDATE: Cell<u32> = const { Cell::new(0) };
}

const WISP_WIN_RELEASE: usize = 0x5749_5350;

pub struct WinKey(HHOOK, HHOOK);

impl WinKey {
    pub fn install(hwnd: HWND) -> windows::core::Result<Self> {
        unsafe {
            let module = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?;
            let keys = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard), Some(module.into()), 0)?;
            match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse), Some(module.into()), 0) {
                Ok(mouse) => {
                    OWNER.set(hwnd);
                    Ok(Self(keys, mouse))
                }
                Err(error) => {
                    let _ = UnhookWindowsHookEx(keys);
                    Err(error)
                }
            }
        }
    }
}

impl Drop for WinKey {
    fn drop(&mut self) {
        OWNER.set(HWND::default());
        CANDIDATE.set(0);
        unsafe {
            let _ = UnhookWindowsHookEx(self.0);
            let _ = UnhookWindowsHookEx(self.1);
        }
    }
}

unsafe extern "system" fn mouse(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0
        && CANDIDATE.get() != 0
        && matches!(
            wp.0 as u32,
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN | WM_MOUSEWHEEL | WM_MOUSEHWHEEL
        )
    {
        CANDIDATE.set(0);
    }
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

unsafe extern "system" fn keyboard(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 {
            let key = &*(lp.0 as *const KBDLLHOOKSTRUCT);
            if key.flags.contains(LLKHF_INJECTED) {
                if key.dwExtraInfo == WISP_WIN_RELEASE
                    && key.flags.contains(LLKHF_UP)
                    && matches!(key.vkCode as u16, key if key == VK_LWIN.0 || key == VK_RWIN.0)
                {
                    let result = CallNextHookEx(None, code, wp, lp);
                    let _ = PostMessageW(Some(OWNER.get()), TOGGLE, WPARAM(0), LPARAM(0));
                    return result;
                }
            } else {
                let vk = key.vkCode;
                if vk == VK_LWIN.0 as u32 || vk == VK_RWIN.0 as u32 {
                    if key.flags.contains(LLKHF_UP) {
                        if CANDIDATE.replace(0) == vk && mask_release(vk) {
                            return LRESULT(1);
                        }
                    } else {
                        let down = |key: VIRTUAL_KEY| GetAsyncKeyState(key.0.into()) < 0;
                        if !down(VIRTUAL_KEY(vk as u16)) {
                            CANDIDATE.set(
                                if [
                                    VK_LWIN,
                                    VK_RWIN,
                                    VK_SHIFT,
                                    VK_CONTROL,
                                    VK_MENU,
                                    VK_LBUTTON,
                                    VK_RBUTTON,
                                    VK_MBUTTON,
                                    VK_XBUTTON1,
                                    VK_XBUTTON2,
                                ]
                                .into_iter()
                                .any(down)
                                {
                                    0
                                } else {
                                    vk
                                },
                            );
                        }
                    }
                } else {
                    CANDIDATE.set(0);
                }
            }
        }
        CallNextHookEx(None, code, wp, lp)
    }
}

/// Mask Explorer's bare Win gesture, then send a balanced Win release.
/// Real Win chords go directly through the hook.
fn mask_release(vk: u32) -> bool {
    let event = |key, flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(key),
                dwFlags: flags,
                dwExtraInfo: WISP_WIN_RELEASE,
                ..Default::default()
            },
        },
    };
    let events = [
        event(0xFF, KEYBD_EVENT_FLAGS(0)),
        event(0xFF, KEYEVENTF_KEYUP),
        event(vk as u16, KEYEVENTF_KEYUP | KEYEVENTF_EXTENDEDKEY),
    ];
    unsafe { SendInput(&events, size_of::<INPUT>() as i32) == events.len() as u32 }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn home_square_cells_keep_each_app_reachable_after_physical_placement() {
        use crate::config::Position;
        for dpi in [96, 120, 144, 192] {
            for (work_width, work_height) in [(1920, 1080), (640, 480)] {
                let work = RECT {
                    left: -1920,
                    top: -600,
                    right: -1920 + work_width,
                    bottom: -600 + work_height,
                };
                for position in [Position::Bottom, Position::Center] {
                    let scale = dpi as f32 / 96.;
                    let full = placement(work, Some(work.bottom - 48), dpi, position);
                    let width = (full.right - full.left) as f32 / scale;
                    let placed = placement_height(work, Some(work.bottom - 48), dpi, position, home_height(width));
                    let height = (placed.bottom - placed.top) as f32 / scale;
                    assert!(
                        placed.left >= work.left
                            && placed.top >= work.top
                            && placed.right <= work.right
                            && placed.bottom <= work.bottom
                    );
                    for index in 0..CAPACITY {
                        let (l, t, r, b) = tile(index, width, height);
                        assert!(r > l && b > t && l >= 0. && r <= width && b < height - FOOTER);
                        assert!(((r - l) - (b - t)).abs() < 0.0001);
                        // Pointer coordinates pass through physical pixels and
                        // a negative monitor origin before the production hit path.
                        let screen_x = placed.left + (((l + r) / 2.) * scale).round() as i32;
                        let screen_y = placed.top + (((t + b) / 2.) * scale).round() as i32;
                        assert_eq!(
                            hit(
                                (screen_x - placed.left) as f32 / scale,
                                (screen_y - placed.top) as f32 / scale,
                                width,
                                height,
                                CAPACITY
                            ),
                            Some(index)
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn home_spacing_and_profile_target_are_compact_without_shrinking_nominal_artwork() {
        let height = home_height(WIDTH);
        assert!(height < HEIGHT);
        let (l, t, r, b) = tile(0, WIDTH, height);
        assert!((pin_icon_size(b - t) - 26.4).abs() < 0.0001);
        let (_, next_top, _, _) = tile(6, WIDTH, height);
        assert!((next_top - b - 4.).abs() < 0.0001);
        assert_eq!(hit((l + r) / 2., (b + next_top) / 2., WIDTH, height, CAPACITY), None);
        for width in [320., WIDTH] {
            let height = home_height(width);
            let mut size = None;
            for index in 0..3 {
                let (l, t, r, b) = footer(index, width, height);
                assert!(((r - l) - (b - t)).abs() < 0.0001);
                assert_eq!(*size.get_or_insert(r - l), r - l);
                assert_eq!(
                    hit((l + r) / 2., (t + b) / 2., width, height, 0),
                    Some(CAPACITY + index)
                );
            }
            // The old wide Profile text strip no longer intercepts empty footer space.
            assert_eq!(hit(100., height - FOOTER / 2., width, height, 0), None);
        }
        assert_eq!(search_height(0, 8, true), HEIGHT);
        assert!(search_height(100, 8, false) > height);
    }
    #[test]
    fn start_opacity_follows_full_transition_without_content_floor() {
        let mut motion = Motion::default();
        motion.retarget_duration(true, 180);
        assert_eq!(motion.render_opacity(), 0.);
        motion.tick(0.08);
        motion.retarget_duration(false, 140);
        motion.tick(0.12);
        assert!(motion.value < 0.08);
        assert_eq!(motion.render_opacity(), motion.value);
        let before = (motion.value, motion.velocity);
        motion.retarget_duration(true, 180);
        assert_eq!((motion.value, motion.velocity), before);
        motion.tick(1.);
        assert_eq!(motion.render_opacity(), 1.);
        motion.retarget_duration(false, 0);
        assert_eq!(motion.render_opacity(), 0.);
    }
    #[test]
    fn pin_artwork_retains_nominal_size_and_clips_in_small_work_areas() {
        assert!((pin_icon_size(108.) - 26.4).abs() < 0.001);
        for height in [0., 10., 24., 48., 108.] {
            assert!(pin_icon_size(height) <= (height - 8.).max(0.));
        }
        let (l, t, r, b) = tile(0, WIDTH, HEIGHT);
        assert!(r - l > pin_icon_size(b - t));
        assert!(b - t > pin_icon_size(b - t));
    }

    fn scoped_app_actions() -> Vec<crate::search::ResultItem> {
        let apps = [crate::indexer::AppEntry {
            name: "Editor".into(),
            id: "editor".into(),
            path: Some(r"C:\Editor.exe".into()),
            desktop: None,
        }];
        let result = crate::search::Searcher::new()
            .search(
                "Editor",
                &apps,
                &crate::history::History::default(),
                &crate::config::Config::default(),
            )
            .remove(0);
        result.actions()
    }

    #[test]
    fn scoped_primary_activation_preserves_admin_intent_and_explicit_override() {
        let actions = scoped_app_actions();
        let open = actions.iter().find(|row| row.title == "Open").unwrap();
        let admin = actions.iter().find(|row| row.title == "Run as administrator").unwrap();
        // Pointer and Enter dispatch both use this production selection helper.
        assert_eq!(result_app_action(open, false), Some(("editor", AppMenuAction::Open)));
        assert_eq!(
            result_app_action(admin, false),
            Some(("editor", AppMenuAction::Administrator))
        );
        assert_eq!(
            result_app_action(open, true),
            Some(("editor", AppMenuAction::Administrator))
        );
        assert_eq!(
            result_app_action(admin, true),
            Some(("editor", AppMenuAction::Administrator))
        );
    }

    #[test]
    fn scoped_reveal_and_copy_keep_their_row_action_without_an_app_menu() {
        let actions = scoped_app_actions();
        for row in &actions {
            let launch = matches!(row.action, crate::search::Action::Launch { .. });
            assert_eq!(row.kind, crate::search::Kind::App);
            // Right-click and keyboard menus accept app launch rows, while
            // scoped Reveal/Copy retain their own primary activation.
            assert_eq!(result_app_action(row, false).is_some(), launch);
        }
        let reveal = actions
            .iter()
            .find(|row| matches!(row.action, crate::search::Action::Reveal(_)))
            .unwrap();
        let copy = actions
            .iter()
            .find(|row| matches!(row.action, crate::search::Action::Copy(_)))
            .unwrap();
        assert_eq!(reveal.enter_label(), "Open");
        assert_eq!(copy.enter_label(), "Copy");
        assert!(result_app_action(reveal, true).is_none());
        assert!(result_app_action(copy, true).is_none());
    }

    #[test]
    fn independent_placement_centers_and_clamps_negative_monitors_and_dpi() {
        use crate::config::Position;
        for dpi in [96, 144, 192] {
            for (w, h) in [(1920, 1080), (640, 480), (320, 240)] {
                let work = RECT {
                    left: -1920,
                    top: -600,
                    right: -1920 + w,
                    bottom: -600 + h,
                };
                for position in [Position::Bottom, Position::Center] {
                    let r = placement(work, Some(work.bottom - 48), dpi, position);
                    assert!(
                        r.left >= work.left && r.top >= work.top && r.right <= work.right && r.bottom <= work.bottom
                    );
                    assert!(r.right > r.left && r.bottom > r.top);
                    assert!(((r.left + r.right) - (work.left + work.right)).abs() <= 1);
                    if position == Position::Center {
                        assert!(((r.top + r.bottom) - (work.top + work.bottom)).abs() <= 1);
                        assert_eq!(r, placement(work, None, dpi, position));
                    } else {
                        assert!(r.bottom < work.bottom - 48);
                    }
                }
            }
        }
    }

    #[test]
    fn grid_search_and_utility_bounds_do_not_overlap() {
        for width in [320., WIDTH] {
            for height in [240., HEIGHT] {
                for i in 0..CAPACITY {
                    let (l, t, r, b) = tile(i, width, height);
                    assert!(l >= 0. && r <= width && r > l && b >= t && b < height - FOOTER);
                }
                for i in 0..rows(height) {
                    assert_eq!(row_at(row_top(i) + 1., rows(height), height), Some(i));
                    assert!(row_top(i) + crate::render::layout::ROW_H <= height - FOOTER - ACTIONS);
                }
                for i in 0..3 {
                    let (l, t, r, b) = inline_rect(i, width, height);
                    assert!(l >= 0. && r <= width && r > l && b > t && b <= height - FOOTER);
                    assert_eq!(row_at((t + b) / 2., usize::MAX, height), None);
                    let (_, t, _, b) = footer(i, width, height);
                    assert_eq!(row_at((t + b) / 2., usize::MAX, height), None);
                }
                assert_eq!(row_at(16., rows(height), height), None);
            }
        }
    }

    #[test]
    fn inline_press_never_retargets_a_changed_app_or_verb() {
        assert!(inline_identity_matches(
            "editor",
            AppMenuAction::Open,
            "editor",
            AppMenuAction::Open
        ));
        assert!(!inline_identity_matches(
            "editor",
            AppMenuAction::Open,
            "browser",
            AppMenuAction::Open
        ));
        assert!(!inline_identity_matches(
            "editor",
            AppMenuAction::Administrator,
            "editor",
            AppMenuAction::Open
        ));
    }

    #[test]
    fn utility_keyboard_traversal_includes_every_available_action_and_returns_to_query() {
        for actions in [0, 1, 3] {
            let mut focus = None;
            for i in CAPACITY..INLINE_BASE + actions {
                focus = utility_focus(focus, actions, false);
                assert_eq!(focus, Some(i));
            }
            assert_eq!(utility_focus(focus, actions, false), None);
            let mut focus = None;
            for i in (CAPACITY..INLINE_BASE + actions).rev() {
                focus = utility_focus(focus, actions, true);
                assert_eq!(focus, Some(i));
            }
            assert_eq!(utility_focus(focus, actions, true), None);
        }
    }

    #[test]
    fn remote_motion_caps_duration_without_resetting_reversal_or_disabled_motion() {
        let mut motion = Motion::default();
        motion.retarget_profile(true, true, true);
        assert_eq!(motion.remaining, 0.12);
        motion.tick(0.04);
        let before = (motion.value, motion.velocity);
        motion.retarget_profile(false, true, true);
        assert_eq!((motion.value, motion.velocity), before);
        assert_eq!(motion.remaining, 0.10);
        motion.tick(0.10);
        assert!(!motion.active());
        assert_eq!(motion.value, 0.);
        motion.retarget_profile(true, false, true);
        assert_eq!(motion.value, 1.);
        assert!(!motion.active());
    }

    #[test]
    fn motion_finishes_in_bounded_time_and_respects_disabled_animation() {
        let mut motion = Motion::default();
        motion.retarget(true, true);
        let mut previous = 0.;
        for _ in 0..14 {
            motion.tick(1. / 60.);
            assert!((previous..=1.).contains(&motion.value));
            previous = motion.value;
        }
        assert!(!motion.active());
        assert_eq!(motion.value, 1.);
        motion.retarget(false, true);
        for _ in 0..11 {
            motion.tick(1. / 60.);
        }
        assert!(!motion.active());
        assert_eq!(motion.value, 0.);
        motion.retarget(true, false);
        assert_eq!(motion.value, 1.);
        assert!(!motion.active());
        motion.retarget(false, false);
        assert_eq!(motion.value, 0.);
        assert!(!motion.tick(1.));
    }

    #[test]
    fn rapid_reversal_preserves_position_and_velocity_without_bounce_or_restart() {
        let mut motion = Motion::default();
        motion.retarget(true, true);
        motion.tick(0.06);
        let before = (motion.value, motion.velocity);
        motion.retarget(false, true);
        assert_eq!((motion.value, motion.velocity), before);
        assert!(motion.closing());
        motion.tick(0.03);
        let before = (motion.value, motion.velocity);
        motion.retarget(true, true);
        assert_eq!((motion.value, motion.velocity), before);
        assert!(!motion.closing());
        for _ in 0..30 {
            motion.tick(1. / 120.);
            assert!((0. ..=1.).contains(&motion.value));
        }
        assert_eq!(motion.value, 1.);
        assert!(!motion.active());
        motion.retarget(false, true);
        motion.tick(2.);
        assert_eq!(motion.value, 0.);
        assert!(!motion.active());
    }

    #[test]
    fn menus_keep_all_verbs_visible_and_disable_missing_capabilities() {
        let app = crate::indexer::AppEntry {
            name: "Store".into(),
            id: "package!app".into(),
            path: None,
            desktop: None,
        };
        let actions = app_menu_actions(false, true);
        assert_eq!(actions.len(), 8);
        assert_eq!(actions[3], AppMenuAction::SetPinned(true));
        assert_eq!(actions[4], AppMenuAction::SetDockPinned(false));
        assert_eq!(disabled_reason(actions[1], &app), Some("Packaged apps cannot elevate"));
        assert_eq!(disabled_reason(actions[2], &app), Some("No local file path"));
        assert!(disabled_reason(actions[0], &app).is_none());
        assert!(resolve_app_action(&app.id, std::slice::from_ref(&app), &actions, Some(1)).is_none());
        assert!(resolve_app_action(&app.id, std::slice::from_ref(&app), &actions, Some(5)).is_none());
    }

    #[test]
    fn previous_start_pin_file_preserves_explicit_order_and_empty_choice() {
        let pins: Pins = toml::from_str("ids = ['editor', 'browser']").unwrap();
        assert_eq!(
            pins.ids.as_deref(),
            Some(["editor".into(), "browser".into()].as_slice())
        );
        let empty: Pins = toml::from_str("ids = []").unwrap();
        assert_eq!(empty.ids, Some(Vec::new()));
    }

    #[test]
    fn compact_search_keeps_rows_actions_footer_and_home_surface_budget() {
        for count in [0, 1, 2, 4, 80] {
            let h = search_height(count, 8, false);
            assert!(h <= HEIGHT);
            assert!(h >= LIST_TOP + FOOTER + ACTIONS + crate::render::layout::ROW_H);
            assert!(rows(h) >= count.min(4));
            for dpi in [96, 144, 192] {
                let work = RECT {
                    left: -1920,
                    top: -600,
                    right: 0,
                    bottom: 480,
                };
                for position in [crate::config::Position::Bottom, crate::config::Position::Center] {
                    let r = placement_height(work, Some(420), dpi, position, h);
                    assert!(r.top >= work.top && r.bottom <= work.bottom);
                    if position == crate::config::Position::Center {
                        assert!(((r.top + r.bottom) - (work.top + work.bottom)).abs() <= 1);
                    }
                }
            }
        }
        assert!(search_height(0, 8, false) < HEIGHT);
        assert!(search_height(1, 8, false) < search_height(2, 8, false));
        assert_eq!(search_height(0, 8, true), HEIGHT);
    }

    #[test]
    fn keyboard_context_menu_anchors_stay_inside_their_selected_row_or_tile() {
        for width in [320., WIDTH] {
            for row in 0..4 {
                let (x, y) = result_menu_anchor(row, width);
                assert!((0. ..width).contains(&x));
                assert!((row_top(row)..row_top(row) + crate::render::layout::ROW_H).contains(&y));
                assert!(y < row_top(row + 1));
            }
            for index in 0..CAPACITY {
                let (l, t, r, b) = tile(index, width, HEIGHT);
                let (x, y) = ((l + r) / 2., (t + b) / 2.);
                assert!((l..r).contains(&x) && (t..b).contains(&y));
            }
        }
    }

    #[test]
    fn native_menu_result_resolves_stable_app_after_reorder_and_rejects_removed_or_changed_app() {
        let app = |id: &str, path: Option<&str>| crate::indexer::AppEntry {
            desktop: None,
            id: id.into(),
            name: id.into(),
            path: path.map(str::to_string),
        };
        let editor = app("editor", Some(r"C:\Editor.exe"));
        assert!(app_snapshot_matches(&editor, &editor, (1, 2), (1, 2)));
        assert!(
            !app_snapshot_matches(&editor, &editor, (1, 2), (1, 3)),
            "a rescan cancels the pending command"
        );
        assert!(
            !app_snapshot_matches(&editor, &editor, (1, 2), (2, 2)),
            "a reopened Start cancels the pending command"
        );
        assert!(!app_snapshot_matches(&editor, &app("editor", None), (1, 2), (1, 2)));
        let actions = app_menu_actions(false, false);
        let apps = vec![app("browser", None), editor];
        let (resolved, action) = resolve_app_action("editor", &apps, &actions, Some(0)).unwrap();
        assert_eq!(resolved.id, "editor");
        assert_eq!(action, AppMenuAction::Open);
        assert!(resolve_app_action("missing", &apps, &actions, Some(0)).is_none());
        assert!(resolve_app_action("editor", &apps, &actions, None).is_none());
        let replaced = vec![app("editor", None)];
        assert!(resolve_app_action("editor", &replaced, &actions, Some(1)).is_none());
    }

    #[test]
    fn pin_intent_is_idempotent_and_failed_save_keeps_previous_pins() -> io::Result<()> {
        let dir = std::env::temp_dir().join(format!("wisp-start-menu-test-{}", std::process::id()));
        std::fs::create_dir(&dir)?;
        let path = dir.join("pins.toml");
        let apps = vec![crate::indexer::AppEntry {
            desktop: None,
            id: "editor".into(),
            name: "Editor".into(),
            path: None,
        }];
        let mut pins = Pins::default();
        pins.set_pinned_to("editor", true, &apps, &path)?;
        pins.set_pinned_to("editor", true, &apps, &path)?;
        assert_eq!(pins.apps(&apps).len(), 1);
        assert!(
            pins.set_pinned_to("editor", false, &apps, &dir.join("missing/pins.toml"))
                .is_err()
        );
        assert_eq!(pins.apps(&apps).len(), 1);
        pins.set_pinned_to("editor", false, &apps, &path)?;
        assert!(pins.apps(&apps).is_empty());
        std::fs::remove_file(path)?;
        std::fs::remove_dir(dir)?;
        Ok(())
    }
}

#[cfg(test)]
mod remote_footer_tests {
    use super::*;
    #[test]
    fn profile_rows_preserve_power_and_map_each_remote_choice() {
        let rows = FooterMenu::Profile.actions();
        assert_eq!(rows[0], FooterAction::Power(Power::Lock));
        assert_eq!(rows[1], FooterAction::Power(Power::SignOut));
        for (index, mode) in crate::remote_mode::Mode::ALL.into_iter().enumerate() {
            assert_eq!(rows[index + 2], FooterAction::Remote(mode));
            let (_, top, _, bottom) = FooterMenu::Profile.row(index + 2, 520., 500.);
            assert!(bottom > top);
        }
        assert_eq!(FooterMenu::Power.actions()[0], FooterAction::Quit);
    }
}
