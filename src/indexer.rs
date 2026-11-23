//! Indexes desktop and Microsoft Store apps from `shell:AppsFolder`,
//! the source for Start's "All apps". The worker thread sends the app list
//! before loading icons so the launcher can show results sooner.
//! Loaded icons stay cached across rescans.

use std::collections::HashSet;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::*;
use windows::Win32::UI::Shell::*;
use windows::core::{HSTRING, Interface, PWSTR, Result};

use crate::worker::WorkerLifetime;

#[path = "indexer/recent.rs"]
pub(crate) mod recent;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DesktopLaunch {
    pub target: String,
    pub arguments: Option<String>,
    pub directory: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AppEntry {
    pub name: String,
    /// Windows shell parsing name in `shell:AppsFolder`: a path or an AppUserModelID.
    pub id: String,
    /// File path for desktop apps, if Windows can resolve it.
    pub path: Option<String>,
    /// Optional shell metadata; absent in older caches and pin files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop: Option<DesktopLaunch>,
}

/// Premultiplied BGRA pixels.
pub struct IconPixels {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

pub enum IndexEvent {
    Apps(Vec<AppEntry>),
    ScanFailed,
    Recent(Vec<recent::RecentApp>),
    Icons(Vec<(String, Arc<IconPixels>)>),
}

pub struct Indexer {
    requests: Sender<()>,
}

impl Indexer {
    /// Posts `notify_msg` to `hwnd` after each event to wake the message loop.
    pub fn spawn(lifetime: &WorkerLifetime, notify_msg: u32, events: Sender<IndexEvent>, icon_size: i32) -> Indexer {
        let (requests, rx) = channel();
        let lifetime = lifetime.clone();
        std::thread::Builder::new()
            .name("indexer".into())
            .spawn(move || worker(rx, events, icon_size, &lifetime, notify_msg))
            .expect("could not start app indexing thread");
        Indexer { requests }
    }

    pub fn rescan(&self) {
        let _ = self.requests.send(());
    }
}

fn worker(
    requests: Receiver<()>,
    events: Sender<IndexEvent>,
    icon_size: i32,
    lifetime: &WorkerLifetime,
    notify_msg: u32,
) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let mut icons_loaded = HashSet::new();
    let mut history = None;
    while requests.recv().is_ok() && lifetime.running() {
        // One scan handles all pending requests.
        while requests.try_recv().is_ok() {}

        let items = match unsafe { enumerate_apps(lifetime) } {
            Ok(items) => items,
            Err(e) => {
                eprintln!("could not list apps: {e}");
                if events.send(IndexEvent::ScanFailed).is_err() {
                    return;
                }
                lifetime.notify(notify_msg);
                continue;
            }
        };
        if !lifetime.running() {
            return;
        }
        let apps = items.iter().map(|(app, _)| app.clone()).collect();
        if events.send(IndexEvent::Apps(apps)).is_err() {
            return;
        }
        lifetime.notify(notify_msg);

        let Some(recent) = scan_recent(&mut history, items.iter().map(|(app, _)| app), lifetime) else {
            return;
        };
        if events.send(IndexEvent::Recent(recent)).is_err() {
            return;
        }
        lifetime.notify(notify_msg);

        let mut batch = Vec::new();
        for (app, item) in &items {
            if !lifetime.running() {
                return;
            }
            if icons_loaded.contains(&app.id) {
                continue;
            }
            // Cache only loaded icons so the next rescan retries failed loads.
            if let Some(pixels) = unsafe { load_icon(item, icon_size) } {
                icons_loaded.insert(app.id.clone());
                batch.push((app.id.clone(), Arc::new(pixels)));
            }
            if batch.len() >= 32 {
                if events.send(IndexEvent::Icons(std::mem::take(&mut batch))).is_err() {
                    return;
                }
                lifetime.notify(notify_msg);
            }
        }
        if !batch.is_empty() {
            let _ = events.send(IndexEvent::Icons(batch));
            lifetime.notify(notify_msg);
        }
    }
}

fn scan_recent<'a>(
    history: &mut Option<recent::History>,
    apps: impl Iterator<Item = &'a AppEntry>,
    lifetime: &WorkerLifetime,
) -> Option<Vec<recent::RecentApp>> {
    // A fast resume can start a new indexer before the old native calls
    // return. Load and save history under the same background-only lock.
    static HISTORY_WRITER: Mutex<()> = Mutex::new(());
    let _writer = HISTORY_WRITER.lock().unwrap_or_else(|error| error.into_inner());
    if !lifetime.running() {
        return None;
    }
    history.get_or_insert_with(recent::History::load).scan(apps, lifetime)
}

#[cfg(test)]
mod tests {
    #[test]
    fn retired_indexer_does_not_open_or_publish_app_history() {
        let lifetime = super::WorkerLifetime::new(windows::Win32::Foundation::HWND::default());
        lifetime.stop();
        let mut history = None;
        assert!(super::scan_recent(&mut history, std::iter::empty(), &lifetime).is_none());
        assert!(history.is_none());
    }
}

unsafe fn enumerate_apps(lifetime: &WorkerLifetime) -> Result<Vec<(AppEntry, IShellItem)>> {
    unsafe {
        let folder: IShellItem = SHGetKnownFolderItem(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, None)?;
        let items: IEnumShellItems = folder.BindToHandler(None, &BHID_EnumItems)?;
        let mut apps = Vec::new();
        let mut seen = HashSet::new();
        while lifetime.running() {
            let mut slot = [None];
            let mut fetched = 0;
            items.Next(&mut slot, Some(&mut fetched))?;
            if fetched == 0 {
                break;
            }
            let Some(item) = slot[0].take() else { break };
            let (Some(name), Some(id)) = (
                display_name(&item, SIGDN_NORMALDISPLAY),
                display_name(&item, SIGDN_PARENTRELATIVEPARSING),
            ) else {
                continue;
            };
            if !is_launchable(&name, &id) || !seen.insert(id.clone()) {
                continue;
            }
            let (path, desktop) = desktop_metadata(&item, &id);
            apps.push((
                AppEntry {
                    name,
                    id,
                    path,
                    desktop,
                },
                item,
            ));
        }
        apps.sort_by_key(|(app, _)| app.name.to_lowercase());
        Ok(apps)
    }
}

/// Excludes uninstallers, documents and web links from app results.
fn is_launchable(name: &str, id: &str) -> bool {
    const SKIP_EXTENSIONS: &[&str] = &[
        ".url", ".txt", ".chm", ".pdf", ".htm", ".html", ".rtf", ".ini", ".log", ".md", ".xml",
    ];
    let id = id.to_ascii_lowercase();
    !name.is_empty()
        && !name.to_ascii_lowercase().starts_with("uninstall")
        && !id.starts_with("http:")
        && !id.starts_with("https:")
        && !SKIP_EXTENSIONS.iter().any(|ext| id.ends_with(ext))
}

unsafe fn display_name(item: &IShellItem, kind: SIGDN) -> Option<String> {
    unsafe {
        let ptr = item.GetDisplayName(kind).ok()?;
        let text = ptr.to_string().ok();
        CoTaskMemFree(Some(ptr.0 as _));
        text
    }
}

/// Desktop app IDs use paths such as `{KnownFolderGUID}\sub\app.exe` or `C:\path\app.exe`.
fn resolve_path(id: &str) -> Option<String> {
    if absolute_desktop_path(id) {
        return Some(id.to_string());
    }
    let rest = id.strip_prefix('{')?;
    let close = rest.find('}')?;
    let guid = unsafe { CLSIDFromString(&HSTRING::from(&id[..close + 2])).ok()? };
    let folder = unsafe {
        let ptr: PWSTR = SHGetKnownFolderPath(&guid, KF_FLAG_DEFAULT, None).ok()?;
        let folder = ptr.to_string().ok();
        CoTaskMemFree(Some(ptr.0 as _));
        folder?
    };
    Some(format!("{folder}{}", &rest[close + 1..]))
}

pub fn absolute_desktop_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    !path.contains('\0')
        && ((bytes.first().is_some_and(u8::is_ascii_alphabetic) && bytes.get(1..3) == Some(b":\\"))
            || path.starts_with(r"\\"))
}

pub fn executable_path(path: &str) -> bool {
    absolute_desktop_path(path)
        && [".exe", ".com", ".lnk"]
            .iter()
            .any(|ext| path.to_ascii_lowercase().ends_with(ext))
}

/// AppsFolder identities are often AUMIDs even for desktop apps. Ask the shell
/// item for its filesystem/link metadata on the existing background worker.
unsafe fn desktop_metadata(item: &IShellItem, id: &str) -> (Option<String>, Option<DesktopLaunch>) {
    unsafe {
        let item2 = item.cast::<IShellItem2>().ok();
        let string = |name: &str| -> Option<String> {
            let mut key = Default::default();
            PropertiesSystem::PSGetPropertyKeyFromName(&HSTRING::from(name), &mut key).ok()?;
            let ptr = item2.as_ref()?.GetString(&key).ok()?;
            let value = ptr.to_string().ok();
            CoTaskMemFree(Some(ptr.0 as _));
            value.filter(|value| !value.is_empty() && !value.contains('\0'))
        };
        let item_path = display_name(item, SIGDN_FILESYSPATH)
            .or_else(|| string("System.ItemPathDisplay"))
            .filter(|path| absolute_desktop_path(path));
        let target = string("System.Link.TargetParsingPath").filter(|path| absolute_desktop_path(path));
        let arguments = string("System.Link.Arguments");
        let directory = string("System.Link.WorkingDirectory").filter(|path| absolute_desktop_path(path));
        let packaged = string("System.AppUserModel.PackageInstallPath").is_some()
            || (id.contains('!') && !absolute_desktop_path(id));
        metadata_paths(item_path, target, resolve_path(id), arguments, directory, packaged)
    }
}

fn metadata_paths(
    item_path: Option<String>,
    target: Option<String>,
    fallback: Option<String>,
    arguments: Option<String>,
    directory: Option<String>,
    packaged: bool,
) -> (Option<String>, Option<DesktopLaunch>) {
    let item_path = item_path.filter(|path| absolute_desktop_path(path));
    let target = target.filter(|path| absolute_desktop_path(path));
    let fallback = fallback.filter(|path| absolute_desktop_path(path));
    let path = target
        .clone()
        .or_else(|| item_path.clone())
        .or_else(|| fallback.clone());
    // Prefer the original shortcut: the shell preserves arguments, working
    // directory and shortcut-specific elevation behavior itself.
    let launch_path = item_path
        .filter(|path| executable_path(path))
        .or_else(|| target.filter(|path| executable_path(path)))
        .or_else(|| fallback.filter(|path| executable_path(path)));
    let desktop = launch_path.filter(|_| !packaged).map(|target| {
        let shortcut = target.to_ascii_lowercase().ends_with(".lnk");
        DesktopLaunch {
            target,
            arguments: if shortcut { None } else { arguments },
            directory: if shortcut { None } else { directory },
        }
    });
    (path, desktop)
}

pub(crate) unsafe fn load_icon(item: &IShellItem, size: i32) -> Option<IconPixels> {
    unsafe {
        let factory: IShellItemImageFactory = item.cast().ok()?;
        let bitmap = factory
            .GetImage(windows::Win32::Foundation::SIZE { cx: size, cy: size }, SIIGBF_ICONONLY)
            .ok()?;
        let pixels = bitmap_pixels(bitmap);
        let _ = DeleteObject(bitmap.into());
        pixels
    }
}

unsafe fn bitmap_pixels(bitmap: HBITMAP) -> Option<IconPixels> {
    unsafe {
        let mut info = BITMAP::default();
        if GetObjectW(
            bitmap.into(),
            size_of::<BITMAP>() as i32,
            Some((&mut info as *mut BITMAP).cast()),
        ) == 0
        {
            return None;
        }
        let (width, height) = (info.bmWidth, info.bmHeight.abs());
        let mut header = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // Negative height stores pixels from top to bottom.
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bgra = vec![0u8; (width * height * 4) as usize];
        let dc = GetDC(None);
        let lines = GetDIBits(
            dc,
            bitmap,
            0,
            height as u32,
            Some(bgra.as_mut_ptr().cast()),
            &mut header,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, dc);
        if lines == 0 {
            return None;
        }

        let (pixels, _) = bgra.as_chunks_mut::<4>();
        if pixels.iter().all(|p| p[3] == 0) {
            // Icons without an alpha channel would be invisible unless made opaque.
            pixels.iter_mut().for_each(|p| p[3] = 255);
        } else if pixels.iter().any(|p| p[0] > p[3] || p[1] > p[3] || p[2] > p[3]) {
            // Direct2D needs premultiplied alpha for these icons.
            for p in pixels.iter_mut() {
                let a = p[3] as u32;
                for c in &mut p[..3] {
                    *c = ((*c as u32 * a + 127) / 255) as u8;
                }
            }
        }
        Some(IconPixels {
            width: width as u32,
            height: height as u32,
            bgra,
        })
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;
    fn text(value: &str) -> Option<String> {
        Some(value.into())
    }

    #[test]
    fn shell_target_works_without_filesystem_display_name_and_preserves_arguments() {
        let (path, desktop) = metadata_paths(
            None,
            text(r"C:\Tools\editor.exe"),
            None,
            text(r#"--profile "Work files""#),
            text(r"C:\Work"),
            false,
        );
        assert_eq!(path.as_deref(), Some(r"C:\Tools\editor.exe"));
        let desktop = desktop.unwrap();
        assert_eq!(desktop.target, r"C:\Tools\editor.exe");
        assert_eq!(desktop.arguments.as_deref(), Some(r#"--profile "Work files""#));
        assert_eq!(desktop.directory.as_deref(), Some(r"C:\Work"));
    }

    #[test]
    fn original_shortcut_keeps_its_own_launch_semantics_and_reveals_target() {
        let (path, desktop) = metadata_paths(
            text(r"C:\Start Menu\editor.lnk"),
            text(r"C:\Tools\editor.exe"),
            None,
            text("--do-not-duplicate"),
            text(r"C:\Work"),
            false,
        );
        assert_eq!(path.as_deref(), Some(r"C:\Tools\editor.exe"));
        let desktop = desktop.unwrap();
        assert_eq!(desktop.target, r"C:\Start Menu\editor.lnk");
        assert_eq!(desktop.arguments, None);
        assert_eq!(desktop.directory, None);
    }

    #[test]
    fn resolved_known_folder_fallback_unc_and_missing_metadata_are_safe() {
        // The already-resolved known-folder fallback enters the same selector;
        // this test does not query machine folders or touch a network share.
        for path in [r"C:\Program Files\Editor\editor.exe", r"\\server\share\editor.exe"] {
            let (reveal, desktop) = metadata_paths(None, None, text(path), None, None, false);
            assert_eq!(reveal.as_deref(), Some(path));
            assert_eq!(desktop.unwrap().target, path);
        }
        assert_eq!(metadata_paths(None, None, None, None, None, false), (None, None));
        assert_eq!(
            metadata_paths(text("relative.exe"), text("package!app"), None, None, None, false),
            (None, None)
        );
        assert!(!absolute_desktop_path("C:\\editor.exe\0injected"));
        assert!(!executable_path(r"C:\Docs\readme.txt"));
    }

    #[test]
    fn packaged_executable_never_becomes_an_elevation_target() {
        let (_, desktop) = metadata_paths(
            text(r"C:\Program Files\WindowsApps\package\app.exe"),
            None,
            None,
            None,
            None,
            true,
        );
        assert!(desktop.is_none());
    }

    #[test]
    fn old_cache_and_pin_app_records_load_without_desktop_metadata() {
        let old = "name = 'Editor'\nid = 'editor'\npath = 'C:\\Editor.exe'\n";
        let app: AppEntry = toml::from_str(old).unwrap();
        assert!(app.desktop.is_none());
        assert_eq!(app.path.as_deref(), Some(r"C:\Editor.exe"));
        let new = AppEntry {
            desktop: Some(DesktopLaunch {
                target: r"C:\Editor.exe".into(),
                arguments: Some("--work".into()),
                directory: None,
            }),
            ..app
        };
        assert_eq!(
            toml::from_str::<AppEntry>(&toml::to_string(&new).unwrap()).unwrap(),
            new
        );
    }
}
