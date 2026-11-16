//! One-shot media-source discovery, called only on the background executor.
//! Reads IDs and Mozilla's installed identities, never track/browser contents.

use crate::media_identity::{self, Source};
use std::{sync::mpsc, time::Duration};
use windows::{
    Media::Control::GlobalSystemMediaTransportControlsSessionManager as Manager,
    Win32::{
        Foundation::{ERROR_NO_MORE_ITEMS, ERROR_SUCCESS, ERROR_TIMEOUT},
        System::{
            Registry::*,
            WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize},
        },
    },
    core::{Error, HRESULT, PWSTR, w},
};

#[derive(Default)]
pub struct Snapshot {
    pub sources: Vec<Source>,
    pub firefox: Vec<String>,
    pub error: Option<String>,
}

trait Discovery {
    fn firefox(&mut self) -> Vec<String>;
    fn ids(&mut self) -> Result<Vec<String>, HRESULT>;
}
fn discover(backend: &mut impl Discovery) -> Snapshot {
    let firefox = media_identity::registered_firefox_ids(backend.firefox());
    match backend.ids() {
        Ok(ids) => Snapshot {
            sources: media_identity::source_list(ids, &firefox),
            firefox,
            error: None,
        },
        Err(code) => {
            crate::logging::error(
                "settings.media_sources",
                format_args!("stage=discovery code={:#010X}", code.0),
            );
            Snapshot {
                firefox,
                error: Some(format!(
                    "Windows media sources are unavailable (0x{:08X}). Refresh to try again.",
                    code.0
                )),
                ..Default::default()
            }
        }
    }
}
pub fn snapshot() -> Snapshot {
    discover(&mut Native)
}

struct Native;
impl Discovery for Native {
    fn firefox(&mut self) -> Vec<String> {
        firefox_registry_ids()
    }
    fn ids(&mut self) -> Result<Vec<String>, HRESULT> {
        media_ids().map_err(|error| error.code())
    }
}

struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            RoUninitialize();
        }
    }
}

fn media_ids() -> windows::core::Result<Vec<String>> {
    unsafe {
        RoInitialize(RO_INIT_MULTITHREADED)?;
    }
    let _apartment = Apartment;
    let operation = Manager::RequestAsync()?;
    let (sender, receiver) = mpsc::sync_channel(1);
    // The projected callback owns only its sender. No borrowed/native context
    // survives cancellation or a closed Settings entity.
    operation.when(move |result| {
        let _ = sender.try_send(result.map_err(|error| error.code()));
    })?;
    let manager = match receiver.recv_timeout(Duration::from_secs(3)) {
        Ok(result) => result.map_err(Error::from_hresult)?,
        Err(_) => {
            let _ = operation.Cancel();
            return Err(Error::from_hresult(HRESULT::from_win32(ERROR_TIMEOUT.0)));
        }
    };
    let sessions = manager.GetSessions()?;
    let mut ids = Vec::new();
    for index in 0..sessions.Size()?.min(128) {
        let Ok(session) = sessions.GetAt(index) else {
            continue;
        };
        let Ok(id) = session.SourceAppUserModelId() else {
            continue;
        };
        if !id.is_empty() && id.len() <= 1024 {
            ids.push(id.to_string());
        }
    }
    Ok(ids)
}

struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}
fn firefox_registry_ids() -> Vec<String> {
    let mut values = Vec::new();
    for hive in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        for view in [KEY_WOW64_32KEY, KEY_WOW64_64KEY] {
            let mut key = HKEY::default();
            if unsafe {
                RegOpenKeyExW(
                    hive,
                    w!("SOFTWARE\\Mozilla\\Firefox\\TaskBarIDs"),
                    None,
                    KEY_QUERY_VALUE | view,
                    &mut key,
                )
            } != ERROR_SUCCESS
            {
                continue;
            }
            let key = Key(key);
            for index in 0..128 {
                let mut name = [0_u16; 1024];
                let mut name_len = name.len() as u32;
                let mut data = [0_u16; 64];
                let mut data_len = size_of_val(&data) as u32;
                let mut kind = 0_u32;
                let code = unsafe {
                    RegEnumValueW(
                        key.0,
                        index,
                        Some(PWSTR(name.as_mut_ptr())),
                        &mut name_len,
                        None,
                        Some(&mut kind),
                        Some(data.as_mut_ptr().cast()),
                        Some(&mut data_len),
                    )
                };
                if code == ERROR_NO_MORE_ITEMS {
                    break;
                }
                if code != ERROR_SUCCESS || kind != REG_SZ.0 || !data_len.is_multiple_of(2) {
                    continue;
                }
                let units = &data[..(data_len as usize / 2).min(data.len())];
                let end = units.iter().position(|unit| *unit == 0).unwrap_or(units.len());
                if let Ok(value) = String::from_utf16(&units[..end]) {
                    values.push(value);
                }
            }
        }
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        ids: Result<Vec<String>, HRESULT>,
        firefox: Vec<String>,
    }
    impl Discovery for Fake {
        fn firefox(&mut self) -> Vec<String> {
            self.firefox.clone()
        }
        fn ids(&mut self) -> Result<Vec<String>, HRESULT> {
            self.ids.clone()
        }
    }
    #[test]
    fn discovery_keeps_registry_identities_on_media_error_and_recovers_on_refresh() {
        let mut backend = Fake {
            ids: Err(HRESULT::from_win32(ERROR_TIMEOUT.0)),
            firefox: vec!["308046B0AF4A39CB".into()],
        };
        let failed = discover(&mut backend);
        assert!(failed.error.is_some());
        assert_eq!(failed.firefox, ["308046B0AF4A39CB"]);
        backend.ids = Ok(vec!["308046B0AF4A39CB".into(), "Spotify.exe".into()]);
        let ready = discover(&mut backend);
        assert!(ready.error.is_none());
        assert!(ready.sources.iter().any(|source| source.name == "Firefox"));
        backend.ids = Ok(Vec::new());
        let empty = discover(&mut backend);
        assert!(empty.sources.is_empty() && empty.error.is_none());
    }
}
