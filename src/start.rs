//! Start menu pins, layout and bare Windows key handling.

use std::{cell::Cell, io};
use windows::{
    Win32::{
        Foundation::*,
        UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
    core::HSTRING,
};

pub const TOGGLE: u32 = WM_APP + 29;
pub const WIDTH: f32 = 544.;
pub const HEIGHT: f32 = 390.;
pub const RADIUS: f32 = 18.;
pub const FOOTER: f32 = 48.;
pub const CAPACITY: usize = 12;
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

#[derive(Default, serde::Serialize, serde::Deserialize)]
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

    pub fn toggle(&mut self, id: &str, apps: &[crate::indexer::AppEntry]) -> io::Result<()> {
        let mut ids = self
            .ids
            .clone()
            .unwrap_or_else(|| self.apps(apps).into_iter().map(|app| app.id.clone()).collect());
        if let Some(index) = ids.iter().position(|pin| pin == id) {
            ids.remove(index);
        } else if ids.len() < CAPACITY {
            ids.push(id.into());
        } else {
            return Err(io::Error::other("Unpin an app before adding another."));
        }
        let next = Self { ids: Some(ids) };
        let path = crate::util::data_dir().join("start-pins.toml");
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, toml::to_string(&next).map_err(io::Error::other)?)?;
        std::fs::rename(tmp, path)?;
        *self = next;
        Ok(())
    }
}

pub fn tile(index: usize, width: f32, height: f32) -> (f32, f32, f32, f32) {
    let w = (width - 32.) / 6.;
    let h = ((height - FOOTER - 94.) / 2.).clamp(24., 88.);
    let x = 16. + (index % 6) as f32 * w;
    let y = 90. + (index / 6) as f32 * h;
    (x + 3., y, x + w - 3., y + h - 4.)
}

pub fn footer(index: usize, width: f32, height: f32) -> (f32, f32, f32, f32) {
    let (left, right) = match index {
        0 => (14., 150.),
        1 => (width - 170., width - 87.),
        _ => (width - 82., width - 14.),
    };
    (left, height - FOOTER + 6., right, height - 6.)
}

use crate::commands::power::Power;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FooterMenu {
    Profile,
    Power,
}

impl FooterMenu {
    pub fn actions(self) -> &'static [Option<Power>] {
        match self {
            Self::Profile => &[Some(Power::Lock), Some(Power::SignOut)],
            Self::Power => &[None, Some(Power::Sleep), Some(Power::Restart), Some(Power::ShutDown)],
        }
    }

    pub fn row(self, index: usize, width: f32, height: f32) -> (f32, f32, f32, f32) {
        let left = if self == Self::Profile { 18. } else { width - 188. };
        let row_height = 31_f32.min((height - FOOTER - 24.) / self.actions().len() as f32);
        let top = height - FOOTER - 14. - (self.actions().len() - index) as f32 * row_height;
        (left, top, left + 170., top + row_height)
    }
}

pub fn menu(hwnd: HWND, anchor: POINT, items: &[&str]) -> Option<usize> {
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        for (i, label) in items.iter().enumerate() {
            let _ = AppendMenuW(menu, MF_STRING, i + 1, &HSTRING::from(*label));
        }
        let picked = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_BOTTOMALIGN,
            anchor.x,
            anchor.y,
            Some(0),
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);
        usize::try_from(picked.0).ok()?.checked_sub(1)
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
