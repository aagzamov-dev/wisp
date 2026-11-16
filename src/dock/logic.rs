//! Matching and DIP geometry.

use crate::config::WindowListMode;
use crate::indexer::AppEntry;

pub const DEFAULT_APP_ICON_SIZE: f32 = 24.0;
pub const MIN_APP_ICON_SIZE: f32 = 24.0;
pub const MAX_APP_ICON_SIZE: f32 = 40.0;
pub const TILE_TOP: f32 = 5.0;
pub const TILE_BOTTOM: f32 = 51.0;
pub const TILE_RADIUS: f32 = 8.0;
pub const ICON_CENTER_Y: f32 = 26.0;
pub const INDICATOR_TOP: f32 = 46.0;

pub fn icon_pixels(dpi: u32, size: f32) -> i32 {
    (size * dpi as f32 / 96.0).round() as i32
}

pub const HEIGHT: f32 = 56.0;
pub const MARGIN: f32 = 9.0;
pub const SLOT: f32 = 48.0;
pub const PAD: f32 = 8.0;
pub const SEPARATOR: f32 = 9.0;

pub fn layout_scale(app_icon_size: f32) -> f32 {
    app_icon_size / 32.0
}

pub fn height(scale: f32) -> f32 {
    HEIGHT * scale
}
/// External spacing is removed only in the fixed Remote-mode layout.
pub fn margin(scale: f32, remote: bool) -> f32 {
    if remote { 0.0 } else { MARGIN * scale }
}

pub fn reserved_height(scale: f32, remote: bool) -> f32 {
    height(scale) + 2.0 * margin(scale, remote)
}

pub fn resting_top(bottom: i32, dpi: u32, app_icon_size: f32, remote: bool) -> i32 {
    let scale = dpi as f32 / 96.;
    let layout = layout_scale(app_icon_size);
    bottom - (height(layout) * scale).round() as i32 - (margin(layout, remote) * scale).round() as i32
}

/// Monitor-wide resting dock band, independent of any animated HWND rectangle.
/// Physical height matches the configured dock and its bottom gap.
pub fn bottom_hover_strip(
    (x, y): (i32, i32),
    (left, top, right, bottom): (i32, i32, i32, i32),
    dpi: u32,
    app_icon_size: f32,
    remote: bool,
) -> bool {
    let band_top = resting_top(bottom, dpi, app_icon_size, remote).max(top);
    (left..right).contains(&x) && (band_top..bottom).contains(&y)
}

/// Hidden-dock activation uses exact physical pixels, never DPI-scaled DIPs.
pub fn bottom_reveal_strip(
    (x, y): (i32, i32),
    (left, top, right, bottom): (i32, i32, i32, i32),
    physical_height: u32,
) -> bool {
    let band = physical_height.clamp(1, 128) as i32;
    (left..right).contains(&x) && (bottom.saturating_sub(band).max(top)..bottom).contains(&y)
}

pub fn bottom_hover_zones(
    cursor: (i32, i32),
    monitor: (i32, i32, i32, i32),
    dpi: u32,
    app_icon_size: f32,
    reveal_height: u32,
    remote: bool,
) -> (bool, bool) {
    let reveal = bottom_reveal_strip(cursor, monitor, reveal_height);
    let hold = bottom_hover_strip(cursor, monitor, dpi, app_icon_size, remote);
    (reveal, reveal || hold)
}

pub fn hover_away_polls(previous: u8, near: bool) -> u8 {
    if near { 0 } else { previous.saturating_add(1) }
}

pub fn reveal_from_hover_strip(inside: bool, mouse_held: bool) -> bool {
    inside && !mouse_held
}
pub fn slot(scale: f32) -> f32 {
    SLOT * scale
}
pub fn pad(scale: f32) -> f32 {
    PAD * scale
}
pub fn separator_width(scale: f32) -> f32 {
    SEPARATOR * scale
}

pub fn width(running: usize) -> f32 {
    2.0 * PAD + (1 + running) as f32 * SLOT
}

pub fn width_scaled(running: usize, scale: f32) -> f32 {
    width(running) * scale
}

pub fn left(index: usize) -> f32 {
    PAD + index as f32 * SLOT
}

pub fn left_scaled(index: usize, scale: f32) -> f32 {
    left(index) * scale
}

pub fn hit(x: f32, y: f32, running: usize) -> Option<usize> {
    if !(0.0..HEIGHT).contains(&y) {
        return None;
    }
    (0..1 + running).find(|&i| (left(i)..left(i) + SLOT).contains(&x))
}

pub fn hit_scaled(x: f32, y: f32, running: usize, scale: f32) -> Option<usize> {
    hit(x / scale, y / scale, running)
}

pub const CHEVRON_W: f32 = 28.0;
pub const TRAY_W: f32 = 32.0;
pub const VOLUME_W: f32 = 46.0;
pub const BATTERY_W: f32 = 50.0;
pub const CLOCK_W: f32 = 88.0;
/// Thumbnail card dimensions in DIPs.
pub const PREVIEW_PAD: f32 = 8.;
pub const PREVIEW_WIDTH: f32 = 168.;
pub const PREVIEW_HEIGHT: f32 = 100.;
pub const PREVIEW_HEADER: f32 = 28.;
pub const PREVIEW_MONITOR: f32 = 17.;
pub const PREVIEW_FOOTER: f32 = 24.;

/// Freeze existing cards for the lifetime of a chooser, across z-order refresh,
/// paging and membership changes. Newly opened windows follow existing cards.
pub fn preview_order(previous: &[isize], current: &[isize]) -> Vec<isize> {
    stable_order(previous.to_vec(), current.to_vec(), |old, new| old == new)
}

pub fn preview_candidates<T>(windows: &[T], local_only: bool, local: impl Fn(&T) -> bool) -> Vec<&T> {
    windows.iter().filter(|window| !local_only || local(window)).collect()
}

pub fn preview_hit(x: f32, y: f32, width: f32, height: f32, count: usize) -> Option<(usize, bool)> {
    let x = x - PREVIEW_PAD;
    let y = y - PREVIEW_PAD;
    if x < 0. || !(0. ..PREVIEW_HEADER + height).contains(&y) || width <= 0. {
        return None;
    }
    let index = (x / width) as usize;
    (index < count).then_some((index, y < PREVIEW_HEADER && x % width >= width - 27.))
}

/// A chooser gesture belongs to the source window, never its changing row index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreviewPress {
    pub hwnd: isize,
    pub pid: u32,
    pub close: bool,
    pub middle: bool,
}
impl PreviewPress {
    pub fn release(self, current: Self) -> Option<(isize, bool)> {
        (self.pid != 0 && self == current).then_some((self.hwnd, self.close || self.middle))
    }
}

/// Prefer a published workspace over the active document in VS Code's standard
/// title format. Other applications retain their own title, including browsers.
pub fn preview_title(title: &str, exe: &str, app: &str) -> String {
    let title = title.trim();
    if title.is_empty() {
        return app.to_string();
    }
    let file = exe.rsplit(['\\', '/']).next().unwrap_or(exe);
    if file.eq_ignore_ascii_case("Code.exe") || file.eq_ignore_ascii_case("Code - Insiders.exe") {
        for app_name in ["Visual Studio Code - Insiders", "Visual Studio Code"] {
            for separator in [" — ", " - ", " – "] {
                let suffix = format!("{separator}{app_name}");
                if let Some(content) = title.strip_suffix(&suffix) {
                    let workspace = content
                        .rsplit_once(separator)
                        .map_or(content, |(_, workspace)| workspace);
                    // A custom title or folder may itself contain the title
                    // delimiter. Only a single boundary or a published absolute
                    // workspace path identifies the folder without guessing.
                    let absolute = workspace.starts_with('/')
                        || workspace.starts_with("\\\\")
                        || workspace.as_bytes().get(1) == Some(&b':');
                    if content.matches(separator).count() > 1 && !absolute {
                        continue;
                    }
                    let folder = workspace
                        .trim()
                        .trim_end_matches(['\\', '/'])
                        .rsplit(['\\', '/'])
                        .next()
                        .unwrap_or_default();
                    if !folder.is_empty() {
                        return folder.to_string();
                    }
                }
            }
        }
    }
    title.to_string()
}

pub fn show_window_list(mode: WindowListMode, windows: usize) -> bool {
    windows > 0 && (mode == WindowListMode::Always || mode == WindowListMode::Multiple && windows > 1)
}
/// Notification-area icons shown in the dock; the rest are behind the chevron.
pub const MAX_INLINE: usize = 5;
/// Standard small notification-area icon size at 100% scaling.
pub const TRAY_ICON: f32 = 16.0;

/// The status parts after the apps, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// Opens the notification-area flyout.
    Chevron,
    /// The nth notification-area icon shown in the dock.
    Tray(usize),
    Volume,
    Battery,
    Clock,
}

impl Part {
    pub fn width(self) -> f32 {
        match self {
            Part::Chevron => CHEVRON_W,
            Part::Tray(_) => TRAY_W,
            Part::Volume => VOLUME_W,
            Part::Battery => BATTERY_W,
            Part::Clock => CLOCK_W,
        }
    }

    pub fn width_scaled(self, scale: f32) -> f32 {
        self.width() * scale
    }
}

/// The parts this PC has: `tray` is the number of inline notification icons
/// while the dock hosts the notification area, volume needs an output
/// device, battery a battery.
pub fn parts(tray: Option<usize>, volume: bool, battery: bool) -> Vec<Part> {
    let tray = tray.map(|count| std::iter::once(Part::Chevron).chain((0..count.min(MAX_INLINE)).map(Part::Tray)));
    tray.into_iter()
        .flatten()
        .chain(
            [(Part::Volume, volume), (Part::Battery, battery), (Part::Clock, true)]
                .into_iter()
                .filter_map(|(part, shown)| shown.then_some(part)),
        )
        .collect()
}

/// Remote uses this same list for drawing, size and hit testing, even without audio.
pub fn remote_parts(mut parts: Vec<Part>) -> Vec<Part> {
    parts.retain(|part| *part != Part::Clock);
    if !parts.contains(&Part::Volume) {
        parts.push(Part::Volume);
    }
    parts
}

/// The notification-area flyout: a grid of cells, as in Windows 11.
pub const FLYOUT_CELL: f32 = 40.0;
pub const FLYOUT_PAD: f32 = 8.0;
pub const FLYOUT_COLUMNS: usize = 5;
/// Space between the flyout and the dock.
pub const FLYOUT_GAP: f32 = 8.0;

/// The flyout's size in DIPs for `count` icons; an empty one keeps one cell.
pub fn flyout_size(count: usize) -> (f32, f32) {
    let columns = count.clamp(1, FLYOUT_COLUMNS);
    let rows = count.max(1).div_ceil(FLYOUT_COLUMNS);
    (
        2.0 * FLYOUT_PAD + columns as f32 * FLYOUT_CELL,
        2.0 * FLYOUT_PAD + rows as f32 * FLYOUT_CELL,
    )
}

/// The cell's top-left corner.
pub fn flyout_cell(index: usize) -> (f32, f32) {
    (
        FLYOUT_PAD + (index % FLYOUT_COLUMNS) as f32 * FLYOUT_CELL,
        FLYOUT_PAD + (index / FLYOUT_COLUMNS) as f32 * FLYOUT_CELL,
    )
}

pub fn flyout_hit(x: f32, y: f32, count: usize) -> Option<usize> {
    if x < FLYOUT_PAD || y < FLYOUT_PAD {
        return None;
    }
    let column = ((x - FLYOUT_PAD) / FLYOUT_CELL) as usize;
    let index = ((y - FLYOUT_PAD) / FLYOUT_CELL) as usize * FLYOUT_COLUMNS + column;
    (column < FLYOUT_COLUMNS && index < count).then_some(index)
}

/// The status parts follow the apps, after a separator.
pub fn status_width(parts: &[Part]) -> f32 {
    if parts.is_empty() {
        return 0.;
    }
    SEPARATOR + parts.iter().map(|part| part.width()).sum::<f32>()
}

pub fn status_width_scaled(parts: &[Part], scale: f32) -> f32 {
    status_width(parts) * scale
}

pub fn total_width(running: usize, parts: &[Part]) -> f32 {
    width(running) + status_width(parts)
}

pub fn total_width_scaled(running: usize, parts: &[Part], scale: f32) -> f32 {
    total_width(running, parts) * scale
}

/// Left edge of `part`, or `None` for a part the PC lacks.
pub fn part_left(part: Part, running: usize, parts: &[Part]) -> Option<f32> {
    let index = parts.iter().position(|&p| p == part)?;
    let start = width(running) - PAD + SEPARATOR;
    Some(start + parts[..index].iter().map(|part| part.width()).sum::<f32>())
}

pub fn part_left_scaled(part: Part, running: usize, parts: &[Part], scale: f32) -> Option<f32> {
    part_left(part, running, parts).map(|left| left * scale)
}

pub fn part_hit(x: f32, y: f32, running: usize, parts: &[Part]) -> Option<Part> {
    if !(0.0..HEIGHT).contains(&y) {
        return None;
    }
    parts
        .iter()
        .copied()
        .find(|&part| part_left(part, running, parts).is_some_and(|left| (left..left + part.width()).contains(&x)))
}

pub fn part_hit_scaled(x: f32, y: f32, running: usize, parts: &[Part], scale: f32) -> Option<Part> {
    part_hit(x / scale, y / scale, running, parts)
}

/// Wheel delta per volume percent, for 2% per 120-delta notch.
pub const WHEEL_PER_PERCENT: i32 = 60;

/// Spends whole percents of the accumulated wheel `delta` on `percent`, and
/// returns the new volume with the leftover delta, for precision touchpads.
pub fn wheel_volume(percent: u32, delta: i32) -> (u32, i32) {
    let steps = delta / WHEEL_PER_PERCENT;
    ((percent as i32 + steps).clamp(0, 100) as u32, delta % WHEEL_PER_PERCENT)
}

/// Wheel travel that switches one desktop: a mouse wheel's notch.
pub const DESKTOP_NOTCH: i32 = 120;

/// Adds wheel `delta` to the `travel` so far, negative toward the right.
/// Returns whether to switch right or left once a notch is complete, and the
/// travel left over. Turning the wheel back starts over.
pub fn wheel_desktop(travel: i32, delta: i32) -> (Option<bool>, i32) {
    let travel = if travel != 0 && (travel < 0) != (delta < 0) {
        delta
    } else {
        travel + delta
    };
    if travel.abs() >= DESKTOP_NOTCH {
        (Some(travel < 0), 0)
    } else {
        (None, travel)
    }
}

fn normalize(path: &str) -> String {
    path.trim()
        .trim_matches('"')
        .replace('/', "\\")
        .trim_start_matches(r"\\?\")
        .to_lowercase()
}

/// Paths must match completely: unrelated programs often share a filename.
pub fn match_app<'a>(apps: &'a [AppEntry], exe: &str, aumid: Option<&str>) -> Option<&'a AppEntry> {
    if let Some(id) = aumid.filter(|s| !s.is_empty())
        && let Some(app) = apps.iter().find(|app| app.id.eq_ignore_ascii_case(id))
    {
        return Some(app);
    }
    if exe.is_empty()
        || exe
            .rsplit(['\\', '/'])
            .next()
            .is_some_and(|s| s.eq_ignore_ascii_case("ApplicationFrameHost.exe"))
    {
        return None;
    }
    let exe = normalize(exe);
    apps.iter()
        .find(|app| app.path.as_deref().is_some_and(|path| normalize(path) == exe) || normalize(&app.id) == exe)
}

/// Keeps items in the order they first appeared. The window list arrives in
/// z-order, which changes whenever an app is focused; following it directly
/// made a clicked icon jump to another place in the dock.
pub fn stable_order<T>(previous: Vec<T>, current: Vec<T>, same: impl Fn(&T, &T) -> bool) -> Vec<T> {
    let mut current = current;
    let mut ordered = Vec::with_capacity(current.len());
    for old in previous {
        if let Some(at) = current.iter().position(|new| same(&old, new)) {
            ordered.push(current.remove(at));
        }
    }
    ordered.append(&mut current);
    ordered
}

#[cfg(test)]
mod tests {
    #[test]
    fn configured_reveal_strip_is_physical_and_keeps_its_entire_activation_area_open() {
        use super::*;
        let monitor = (-1920, -1080, 0, 0);
        for reveal_height in [1, 8, 128] {
            for dpi in [96, 120, 144, 192] {
                for x in [-1920, -960, -1] {
                    let top = -(reveal_height as i32);
                    let (reveal, hold) = bottom_hover_zones((x, top), monitor, dpi, 24., reveal_height, false);
                    assert!(reveal && hold);
                    assert!(!bottom_hover_zones((x, top - 1), monitor, dpi, 24., reveal_height, false).0);
                    assert!(!bottom_hover_zones((x, 0), monitor, dpi, 24., reveal_height, false).0);
                    let mut away = 0;
                    for _ in 0..20 {
                        away = hover_away_polls(away, hold);
                    }
                    assert_eq!(away, 0);
                }
            }
        }
        assert!(!bottom_reveal_strip((0, -1), monitor, 8));
        assert!(!bottom_reveal_strip((-1921, -1), monitor, 8));
        // A point above the 8px trigger holds an already visible dock, but
        // cannot reveal a hidden one, even at higher Windows scaling.
        for dpi in [96, 192] {
            assert_eq!(
                bottom_hover_zones((-1000, -20), monitor, dpi, 24., 8, false),
                (false, true)
            );
        }
        // Tall activation areas must remain part of hold even above the dock.
        assert!(!bottom_hover_strip((-1000, -100), monitor, 96, 24., false));
        assert_eq!(
            bottom_hover_zones((-1000, -100), monitor, 96, 24., 128, false),
            (true, true)
        );
        assert!(!bottom_reveal_strip((0, 0), (0, 0, 0, 0), 8));
    }
    #[test]
    fn preview_monitor_filter_runs_before_count_order_and_click_choice() {
        use super::*;
        let windows = [(11, 1), (22, 2), (33, 1)];
        let local = preview_candidates(&windows, true, |window| window.1 == 1);
        assert_eq!(local.iter().map(|window| window.0).collect::<Vec<_>>(), [11, 33]);
        let one_local = preview_candidates(&windows, true, |window| window.1 == 2);
        assert_eq!(one_local.len(), 1);
        assert_eq!(
            one_local[0].0, 22,
            "one local window must be the direct target, even if remote windows precede it"
        );
        assert!(preview_candidates(&windows, true, |window| window.1 == 3).is_empty());
        assert_eq!(preview_candidates(&windows, false, |_| false).len(), 3);
        let moved = [(11, 2), (22, 2), (33, 1)];
        let local_after_move = preview_candidates(&moved, true, |window| window.1 == 1);
        let ids = local_after_move.iter().map(|window| window.0).collect::<Vec<_>>();
        assert_eq!(preview_order(&[11, 33], &ids), [33]);
    }
    #[test]
    fn monitor_wide_bottom_hover_does_not_oscillate_beside_dock() {
        use super::*;
        let monitor = (0, 0, 1920, 1080);
        for x in [0, 120, 960, 1800, 1919] {
            for y in [1040, 1079] {
                let mut revealed = false;
                let mut away = 0;
                for _ in 0..20 {
                    let inside = bottom_hover_strip((x, y), monitor, 96, 24., false);
                    if !revealed {
                        revealed = reveal_from_hover_strip(inside, false);
                    } else {
                        away = hover_away_polls(away, inside);
                        assert!(away < 3, "a stationary bottom-strip pointer must keep the dock open");
                    }
                    assert!(revealed);
                }
            }
        }
        // The usual three outside polls still hide, and re-entering resets it.
        let mut away = 0;
        for expected in 1..=3 {
            away = hover_away_polls(away, bottom_hover_strip((20, 900), monitor, 96, 24., false));
            assert_eq!(away, expected);
        }
        assert_eq!(hover_away_polls(away, true), 0);
        assert!(
            !reveal_from_hover_strip(true, true),
            "dragging near the bottom must not newly reveal it"
        );
        assert!(!reveal_from_hover_strip(false, false));
        assert!(reveal_from_hover_strip(true, false));
    }

    #[test]
    fn remote_removes_only_external_vertical_space_and_local_return_restores_it() {
        use super::*;
        for (icon_size, body, gap) in [(24., 42., 6.75), (32., 56., 9.), (40., 70., 11.25)] {
            let layout = layout_scale(icon_size);
            assert_eq!(height(layout), body);
            assert_eq!(reserved_height(layout, true), body);
            assert_eq!(reserved_height(layout, false), body + 2. * gap);
            for dpi in [96, 120, 144, 192] {
                let physical = dpi as f32 / 96.;
                let pixels = (body * physical).round() as i32;
                let gap_pixels = (gap * physical).round() as i32;
                // Negative monitor coordinates cover displays above the primary.
                let monitor = (-1920, -1080, 0, 0);
                for remote in [false, true, false] {
                    let top = resting_top(0, dpi, icon_size, remote);
                    assert_eq!(top, -pixels - if remote { 0 } else { gap_pixels });
                    assert!(bottom_hover_strip((-960, top), monitor, dpi, icon_size, remote));
                    assert!(!bottom_hover_strip((-960, top - 1), monitor, dpi, icon_size, remote));
                    assert!(bottom_hover_strip((-960, -1), monitor, dpi, icon_size, remote));
                    assert!(!bottom_hover_strip((-960, 0), monitor, dpi, icon_size, remote));
                    assert_eq!(
                        bottom_hover_zones((-960, -127), monitor, dpi, icon_size, 128, remote),
                        (true, true)
                    );
                }
            }
        }
    }

    #[test]
    fn bottom_hover_band_matches_configured_height_gap_and_dpi() {
        use super::*;
        for dpi in [96, 120, 144, 192] {
            for icon_size in [24., 32., 40.] {
                let scale = dpi as f32 / 96.;
                let layout = layout_scale(icon_size);
                let expected = (height(layout) * scale).round() as i32 + (margin(layout, false) * scale).round() as i32;
                let monitor = (-2560, -1440, 0, 0);
                for x in [-2560, -1280, -1] {
                    assert!(bottom_hover_strip((x, -expected), monitor, dpi, icon_size, false));
                    assert!(bottom_hover_strip((x, -1), monitor, dpi, icon_size, false));
                    assert!(!bottom_hover_strip((x, -expected - 1), monitor, dpi, icon_size, false));
                    assert!(!bottom_hover_strip((x, 0), monitor, dpi, icon_size, false));
                }
                assert!(!bottom_hover_strip((-2561, -1), monitor, dpi, icon_size, false));
                assert!(!bottom_hover_strip((0, -1), monitor, dpi, icon_size, false));
            }
        }
        assert!(!bottom_hover_strip((0, 0), (0, 0, 0, 0), 96, 24., false));
    }
    #[test]
    fn chooser_order_survives_vscode_z_order_refresh_and_second_card_stays_bound() {
        use super::*;
        let opened = preview_order(&[], &[11, 22]);
        let refreshed = preview_order(&opened, &[22, 11]);
        assert_eq!(refreshed, [11, 22]);
        let (row, close) = preview_hit(
            PREVIEW_PAD + PREVIEW_WIDTH * 1.5,
            60.,
            PREVIEW_WIDTH,
            PREVIEW_HEIGHT,
            refreshed.len(),
        )
        .unwrap();
        let pressed = PreviewPress {
            hwnd: opened[row],
            pid: 9,
            close,
            middle: false,
        };
        let released = PreviewPress {
            hwnd: refreshed[row],
            ..pressed
        };
        assert_eq!(pressed.release(released), Some((22, false)));
        // Rebuilding a page after a new window appears retains old positions.
        let added = preview_order(&refreshed, &[33, 22, 11]);
        assert_eq!(added, [11, 22, 33]);
        assert_eq!(preview_order(&added, &[33, 22]), [22, 33]);
    }

    #[test]
    fn vscode_folder_titles_handle_paths_insiders_and_document_separators() {
        use super::preview_title;
        assert_eq!(
            preview_title(
                "read - working tree - C:\\Projects\\Wisp - Visual Studio Code",
                "Code.exe",
                "Code"
            ),
            "Wisp"
        );
        assert_eq!(
            preview_title("readme — /home/dev/Wisp/ — Visual Studio Code", "Code.exe", "Code"),
            "Wisp"
        );
        assert_eq!(
            preview_title(
                "main.rs - C:\\Projects\\Wisp - Visual Studio Code - Insiders",
                "Code - Insiders.exe",
                "Code"
            ),
            "Wisp"
        );
        assert_eq!(preview_title("Custom title", "Code.exe", "Code"), "Custom title");
        let ambiguous = "readme - my - folder - Visual Studio Code";
        assert_eq!(preview_title(ambiguous, "Code.exe", "Code"), ambiguous);
        let path_with_delimiter = "readme - C:\\Projects\\my - folder - Visual Studio Code";
        assert_eq!(
            preview_title(path_with_delimiter, "Code.exe", "Code"),
            path_with_delimiter
        );
        assert_eq!(
            preview_title("readme - Visual Studio Code", "Code.exe", "Code"),
            "readme"
        );
        assert_eq!(
            preview_title("my-project - Visual Studio Code", "Code.exe", "Code"),
            "my-project"
        );
        assert_eq!(
            preview_title("C:\\Projects\\Wisp - Visual Studio Code", "Code.exe", "Code"),
            "Wisp"
        );
        assert_eq!(
            preview_title("/home/dev/Wisp/ — Visual Studio Code", "Code.exe", "Code"),
            "Wisp"
        );
        assert_eq!(
            preview_title(
                "C:\\Projects\\Wisp - Visual Studio Code - Insiders",
                "Code - Insiders.exe",
                "Code"
            ),
            "Wisp"
        );
    }
    #[test]
    fn second_thumbnail_hit_stays_second_at_supported_dpi() {
        use super::*;
        for dpi in [96., 120., 144., 192.] {
            let scale = dpi / 96.;
            let x = ((PREVIEW_PAD + PREVIEW_WIDTH * 1.5) * scale) as i32;
            let y = ((PREVIEW_PAD + PREVIEW_HEADER + 40.) * scale) as i32;
            assert_eq!(
                preview_hit(x as f32 / scale, y as f32 / scale, PREVIEW_WIDTH, PREVIEW_HEIGHT, 2),
                Some((1, false))
            );
        }
        assert_eq!(preview_hit(0., 40., PREVIEW_WIDTH, PREVIEW_HEIGHT, 2), None);
        assert_eq!(
            preview_hit(PREVIEW_PAD + 2. * PREVIEW_WIDTH, 40., PREVIEW_WIDTH, PREVIEW_HEIGHT, 2),
            None
        );
    }
    #[test]
    fn preview_gesture_preserves_window_identity_not_row_order() {
        use super::PreviewPress;
        let selected = PreviewPress {
            hwnd: 22,
            pid: 2,
            close: false,
            middle: false,
        };
        assert_eq!(selected.release(selected), Some((22, false)));
        assert_eq!(selected.release(PreviewPress { hwnd: 11, ..selected }), None);
        assert_eq!(selected.release(PreviewPress { pid: 3, ..selected }), None);
        assert_eq!(
            selected.release(PreviewPress {
                close: true,
                ..selected
            }),
            None
        );
        assert_eq!(
            selected.release(PreviewPress {
                middle: true,
                ..selected
            }),
            None
        );
        assert_eq!(
            PreviewPress { pid: 0, ..selected }.release(PreviewPress { pid: 0, ..selected }),
            None
        );
        let close = PreviewPress {
            close: true,
            ..selected
        };
        assert_eq!(close.release(close), Some((22, true)));
    }

    #[test]
    fn preview_names_use_published_workspace_without_guessing_accounts() {
        use super::preview_title;
        assert_eq!(
            preview_title(
                "appsettings.json - Messenger - Visual Studio Code",
                "C:\\Code.exe",
                "Code"
            ),
            "Messenger"
        );
        assert_eq!(
            preview_title("readme — Wisp — Visual Studio Code", "Code.exe", "Code"),
            "Wisp"
        );
        assert_eq!(
            preview_title("Video - YouTube - Google Chrome", "chrome.exe", "Chrome"),
            "Video - YouTube - Google Chrome"
        );
        assert_eq!(preview_title("Alice", "Telegram.exe", "Telegram"), "Alice");
        assert_eq!(preview_title("Downloads", "explorer.exe", "Explorer"), "Downloads");
        assert_eq!(preview_title("  ", "unknown.exe", "App"), "App");
    }
    use super::*;

    #[test]
    fn window_list_modes_follow_the_selected_policy() {
        assert!(show_window_list(WindowListMode::Always, 1));
        assert!(show_window_list(WindowListMode::Always, 3));
        assert!(!show_window_list(WindowListMode::Multiple, 1));
        assert!(show_window_list(WindowListMode::Multiple, 2));
        assert!(!show_window_list(WindowListMode::Off, 1));
        assert!(!show_window_list(WindowListMode::Off, 2));
        assert!(!show_window_list(WindowListMode::Always, 0));
    }

    #[test]
    fn app_icon_pixel_size_scales_with_dpi() {
        assert_eq!(icon_pixels(96, 24.0), 24);
        assert_eq!(icon_pixels(144, DEFAULT_APP_ICON_SIZE), 36);
        assert_eq!(icon_pixels(192, MAX_APP_ICON_SIZE), 80);
    }
    #[test]
    fn dock_geometry_scales_with_app_icon_size() {
        let small = layout_scale(MIN_APP_ICON_SIZE);
        let normal = layout_scale(32.0);
        let large = layout_scale(MAX_APP_ICON_SIZE);
        assert_eq!((small, normal, large), (0.75, 1.0, 1.25));
        assert_eq!(height(small), 42.0);
        assert_eq!(slot(small), 36.0);
        assert_eq!(height(large), 70.0);
        assert_eq!(slot(large), 60.0);

        for scale in [small, normal, large] {
            let parts = parts(None, true, true);
            assert_eq!(total_width_scaled(2, &parts, scale), total_width(2, &parts) * scale);
            let x = left_scaled(1, scale) + slot(scale) / 2.0;
            assert_eq!(hit_scaled(x, height(scale) / 2.0, 2, scale), Some(1));
        }
    }

    #[test]
    fn scaled_slot_edges_match_hit_targets() {
        for size in [24.0, 28.0, 32.0, 36.0, 40.0] {
            let scale = layout_scale(size);
            for index in 0..=8 {
                let left = left_scaled(index, scale);
                let right = left + slot(scale);
                let bottom = height(scale);
                assert_eq!(left, (8.0 + index as f32 * 48.0) * size / 32.0);
                assert_eq!(right, left_scaled(index + 1, scale));
                assert_eq!(bottom, 56.0 * size / 32.0);
                assert_eq!(hit_scaled(left, 0.0, 8, scale), Some(index));
                assert_eq!(hit_scaled(right - 0.5, bottom - 0.5, 8, scale), Some(index));
                assert_eq!(hit_scaled(left, bottom, 8, scale), None);
                assert_ne!(hit_scaled(right, 0.0, 8, scale), Some(index));
            }
        }
    }

    #[test]
    fn stable_order_keeps_first_seen_positions() {
        let previous = vec!["a", "b", "c"];
        // Focusing c makes it first in z-order; it must stay third.
        let current = vec!["c", "a", "d", "b"];
        assert_eq!(stable_order(previous, current, |a, b| a == b), ["a", "b", "c", "d"]);
        // Closed apps drop out and leaving ones keep their relative order.
        assert_eq!(stable_order(vec!["a", "b"], vec!["b", "c"], |a, b| a == b), ["b", "c"]);
    }
    #[test]
    fn matches_full_paths_and_store_ids() {
        let apps = vec![
            AppEntry {
                desktop: None,
                id: "{folder}\\a.exe".into(),
                path: Some(r"C:\Apps\a.exe".into()),
                name: "A".into(),
            },
            AppEntry {
                desktop: None,
                id: "Package!App".into(),
                path: None,
                name: "Store".into(),
            },
        ];
        assert_eq!(match_app(&apps, r"\\?\C:\APPS\A.EXE", None).unwrap().name, "A");
        assert!(match_app(&apps, r"D:\Other\a.exe", None).is_none());
        assert_eq!(
            match_app(&apps, r"C:\Windows\ApplicationFrameHost.exe", Some("package!app"))
                .unwrap()
                .name,
            "Store"
        );
        assert!(match_app(&apps, "", None).is_none());
    }
    #[test]
    fn wheel_steps_volume_by_two_percent_within_range() {
        assert_eq!(wheel_volume(50, 120), (52, 0));
        assert_eq!(wheel_volume(50, -360), (44, 0));
        assert_eq!(wheel_volume(99, 120), (100, 0));
        assert_eq!(wheel_volume(1, -120), (0, 0));
        // Precision touchpads send fractions of a notch, which add up.
        assert_eq!(wheel_volume(50, 40), (50, 40));
        assert_eq!(wheel_volume(50, 40 + 40), (51, 20));
        assert_eq!(wheel_volume(50, -70), (49, -10));
    }
    #[test]
    fn wheel_switches_one_desktop_a_notch() {
        // Down and touchpad swipes right go to the desktop on the right.
        assert_eq!(wheel_desktop(0, -120), (Some(true), 0));
        assert_eq!(wheel_desktop(0, 120), (Some(false), 0));
        // A fast flick still switches only once.
        assert_eq!(wheel_desktop(0, -480), (Some(true), 0));
        // Precision touchpads send fractions of a notch, which add up.
        assert_eq!(wheel_desktop(0, -50), (None, -50));
        assert_eq!(wheel_desktop(-100, -50), (Some(true), 0));
        // Turning back starts over.
        assert_eq!(wheel_desktop(-100, 30), (None, 30));
    }
    #[test]
    fn floating_layout_and_hit_targets() {
        assert_eq!((HEIGHT, MARGIN, SLOT), (56.0, 9.0, 48.0));
        assert_eq!(width(2), 160.0);
        for count in [0, 1, 8, 30] {
            for index in 0..=count {
                assert_eq!(hit(left(index) + SLOT / 2.0, HEIGHT / 2.0, count), Some(index));
            }
            assert_eq!(hit(-1.0, 20.0, count), None);
            assert_eq!(hit(PAD, HEIGHT, count), None);
            let all = parts(None, true, true);
            let clock = part_left(Part::Clock, count, &all).unwrap();
            assert_eq!(clock + CLOCK_W + PAD, total_width(count, &all));
            let volume = part_left(Part::Volume, count, &all).unwrap();
            assert_eq!(part_hit(volume - 1.0, 20.0, count, &all), None);
            assert_eq!(part_hit(clock + 1.0, 20.0, count, &all), Some(Part::Clock));
            assert_eq!(part_hit(clock - 1.0, 20.0, count, &all), Some(Part::Battery));
            assert_eq!(part_hit(clock, HEIGHT, count, &all), None);
            assert_eq!(part_left(Part::Volume, count, &parts(None, false, false)), None);
        }
        for dpi in [96, 120, 144] {
            assert_eq!(
                icon_pixels(dpi, DEFAULT_APP_ICON_SIZE) as f32,
                DEFAULT_APP_ICON_SIZE * dpi as f32 / 96.0
            );
        }
        assert_eq!(((HEIGHT + MARGIN) * 120.0 / 96.0).round() as i32, 81);
    }
    #[test]
    fn tray_sits_between_apps_and_status() {
        let tray = parts(Some(2), true, true);
        assert_eq!(
            tray,
            [
                Part::Chevron,
                Part::Tray(0),
                Part::Tray(1),
                Part::Volume,
                Part::Battery,
                Part::Clock
            ]
        );
        // The chevron starts right after the apps' separator.
        let chevron = part_left(Part::Chevron, 2, &tray).unwrap();
        assert_eq!(chevron, width(2) - PAD + SEPARATOR);
        assert_eq!(part_left(Part::Tray(0), 2, &tray), Some(chevron + CHEVRON_W));
        let volume = part_left(Part::Volume, 2, &tray).unwrap();
        assert_eq!(volume, chevron + CHEVRON_W + 2.0 * TRAY_W);
        assert_eq!(part_hit(chevron + 1.0, 20.0, 2, &tray), Some(Part::Chevron));
        assert_eq!(part_hit(volume - 1.0, 20.0, 2, &tray), Some(Part::Tray(1)));
        assert_eq!(
            total_width(2, &tray),
            total_width(2, &parts(None, true, true)) + CHEVRON_W + 2.0 * TRAY_W
        );
        // Inline icons are capped; with none, only the chevron shows.
        assert_eq!(parts(Some(9), false, false).len(), 1 + MAX_INLINE + 1);
        assert_eq!(parts(Some(0), false, false), [Part::Chevron, Part::Clock]);
        // Tray icons fit the dock's height budget.
        const { assert!(TRAY_ICON < HEIGHT && TRAY_ICON < TRAY_W) };
    }
    #[test]
    fn flyout_grid_wraps_at_five() {
        assert_eq!(flyout_size(0), flyout_size(1));
        assert_eq!(
            flyout_size(3),
            (2.0 * FLYOUT_PAD + 3.0 * FLYOUT_CELL, 2.0 * FLYOUT_PAD + FLYOUT_CELL)
        );
        assert_eq!(
            flyout_size(6),
            (
                2.0 * FLYOUT_PAD + 5.0 * FLYOUT_CELL,
                2.0 * FLYOUT_PAD + 2.0 * FLYOUT_CELL
            )
        );
        assert_eq!(flyout_cell(6), (FLYOUT_PAD + FLYOUT_CELL, FLYOUT_PAD + FLYOUT_CELL));
        let (x, y) = flyout_cell(6);
        assert_eq!(flyout_hit(x + 1.0, y + 1.0, 7), Some(6));
        assert_eq!(flyout_hit(x + 1.0, y + 1.0, 6), None);
        assert_eq!(flyout_hit(FLYOUT_PAD - 1.0, 10.0, 3), None);
        assert_eq!(flyout_hit(FLYOUT_PAD + 5.0 * FLYOUT_CELL + 1.0, 10.0, 9), None);
    }
}

#[cfg(test)]
mod remote_tests {
    use super::*;
    #[test]
    fn remote_controls_without_audio_share_clock_free_geometry() {
        let remote = remote_parts(parts(Some(2), false, false));
        assert_eq!(remote, [Part::Chevron, Part::Tray(0), Part::Tray(1), Part::Volume]);
        assert_eq!(part_left(Part::Clock, 3, &remote), None);
        let controls = part_left(Part::Volume, 3, &remote).unwrap();
        assert_eq!(part_hit(controls + 1., 20., 3, &remote), Some(Part::Volume));
        assert_eq!(remote_parts(parts(None, true, false)), [Part::Volume]);
    }
}
