//! Date metadata for the current Start catalog. Runs on the indexer thread.

use std::{collections::BTreeMap, path::PathBuf};
use windows::{
    Win32::{
        Foundation::{FILETIME, SYSTEMTIME},
        System::{Registry::*, SystemInformation::GetLocalTime, Time::SystemTimeToFileTime},
    },
    core::{PCWSTR, PWSTR, w},
};

use super::{AppEntry, absolute_desktop_path};
use crate::worker::WorkerLifetime;

#[derive(Debug)]
pub struct RecentApp {
    pub app_index: usize,
    pub date_label: String,
}

pub struct History {
    path: PathBuf,
    // Zero records membership in the initial baseline without guessing a date.
    seen: Option<BTreeMap<String, u32>>,
    dirty: bool,
}

impl History {
    pub fn load() -> Self {
        let path = crate::util::data_dir().join("start-app-history.toml");
        let seen = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| toml::from_str(&text).ok());
        Self {
            path,
            seen,
            dirty: false,
        }
    }

    pub fn scan<'a>(
        &mut self,
        apps: impl Iterator<Item = &'a AppEntry>,
        lifetime: &WorkerLifetime,
    ) -> Option<Vec<RecentApp>> {
        if !lifetime.running() {
            return None;
        }
        let apps: Vec<_> = apps.collect();
        let now = unsafe { GetLocalTime() };
        let today = u32::from(now.wYear) * 10_000 + u32::from(now.wMonth) * 100 + u32::from(now.wDay);
        if !apps.is_empty() {
            let first_seen_date = if self.seen.is_some() { today } else { 0 };
            let seen = self.seen.get_or_insert_with(BTreeMap::new);
            self.dirty |= record_seen(seen, &apps, first_seen_date);
            if self.dirty {
                let tmp = self.path.with_extension(format!("{}.tmp", std::process::id()));
                if let Ok(text) = toml::to_string(seen)
                    && std::fs::write(&tmp, text).is_ok()
                    && std::fs::rename(&tmp, &self.path).is_ok()
                {
                    self.dirty = false;
                }
            }
        }

        let registrations = registrations(today, lifetime);
        lifetime
            .running()
            .then(|| recent_rows(&apps, &registrations, self.seen.as_ref(), today))
    }
}

fn record_seen(seen: &mut BTreeMap<String, u32>, apps: &[&AppEntry], date: u32) -> bool {
    let old_len = seen.len();
    for app in apps {
        seen.entry(app.id.to_lowercase()).or_insert(date);
    }
    seen.len() != old_len
}

struct Registration {
    name: String,
    location: Option<String>,
    date: Option<(u32, &'static str)>,
}

fn recent_rows(
    apps: &[&AppEntry],
    registrations: &[Registration],
    seen: Option<&BTreeMap<String, u32>>,
    today: u32,
) -> Vec<RecentApp> {
    let mut names = BTreeMap::new();
    for app in apps {
        *names.entry(app.name.trim().to_lowercase()).or_insert(0) += 1;
    }
    let mut dated = Vec::new();
    for (index, app) in apps.iter().enumerate() {
        let name = app.name.trim().to_lowercase();
        let date = registration_date(app, &name, names[&name] == 1, registrations).or_else(|| {
            let date = *seen?.get(&app.id.to_lowercase())?;
            (date != 0)
                .then(|| parse_date(&date.to_string(), today))
                .flatten()
                .map(|date| (date, "First seen"))
        });
        if let Some((date, source)) = date {
            dated.push((date, index, source));
        }
    }
    // Apps arrive sorted by name, so ties retain that order.
    dated.sort_by_key(|&(date, index, _)| (std::cmp::Reverse(date), index));
    dated
        .into_iter()
        .map(|(date, app_index, source)| RecentApp {
            app_index,
            date_label: format!(
                "{source} · {:04}-{:02}-{:02}",
                date / 10_000,
                date / 100 % 100,
                date % 100
            ),
        })
        .collect()
}

fn registration_date(
    app: &AppEntry,
    name: &str,
    unique_name: bool,
    registrations: &[Registration],
) -> Option<(u32, &'static str)> {
    let path = app
        .path
        .as_deref()
        .filter(|path| absolute_desktop_path(path))?
        .to_lowercase();
    if app.desktop.is_none() || path.ends_with(".lnk") {
        return None;
    }
    let named = registrations.iter().filter(|entry| entry.name == name);
    let located = |entry: &Registration| {
        entry
            .location
            .as_ref()
            .is_some_and(|location| path.starts_with(location))
    };
    let has_located = named.clone().any(located);
    let mut matches = named.filter(|entry| {
        if has_located {
            located(entry)
        } else {
            unique_name && entry.location.is_none()
        }
    });
    if !has_located
        && registrations
            .iter()
            .any(|entry| entry.name == name && entry.location.is_some())
    {
        // An explicit, conflicting location makes a label-only match unsafe.
        return None;
    }
    let first = matches.next()?.date?;
    // Multiple legitimate registrations can share a name. Accept duplicates
    // only when their Windows date and its meaning agree.
    matches.all(|entry| entry.date == Some(first)).then_some(first)
}

fn parse_date(value: &str, today: u32) -> Option<u32> {
    if value.len() != 8 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let date: u32 = value.parse().ok()?;
    if date > today {
        return None;
    }
    let time = SYSTEMTIME {
        wYear: (date / 10_000) as u16,
        wMonth: (date / 100 % 100) as u16,
        wDay: (date % 100) as u16,
        ..Default::default()
    };
    let mut file_time = FILETIME::default();
    unsafe { SystemTimeToFileTime(&time, &mut file_time).ok()? };
    Some(date)
}

fn registrations(today: u32, lifetime: &WorkerLifetime) -> Vec<Registration> {
    let mut entries = Vec::new();
    let uninstall = w!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall");
    for (hive, view, query_view) in [
        (HKEY_CURRENT_USER, KEY_WOW64_64KEY, RRF_SUBKEY_WOW6464KEY),
        (HKEY_CURRENT_USER, KEY_WOW64_32KEY, RRF_SUBKEY_WOW6432KEY),
        (HKEY_LOCAL_MACHINE, KEY_WOW64_64KEY, RRF_SUBKEY_WOW6464KEY),
        (HKEY_LOCAL_MACHINE, KEY_WOW64_32KEY, RRF_SUBKEY_WOW6432KEY),
    ] {
        if !lifetime.running() {
            break;
        }
        let mut key = HKEY::default();
        if unsafe { RegOpenKeyExW(hive, uninstall, None, KEY_READ | view, &mut key) }.is_err() {
            continue;
        }
        for index in 0.. {
            if !lifetime.running() {
                break;
            }
            // Registry subkey names have a documented maximum of 255 chars.
            let mut sub = [0_u16; 256];
            let mut len = sub.len() as u32;
            let status = unsafe {
                RegEnumKeyExW(
                    key,
                    index,
                    Some(PWSTR(sub.as_mut_ptr())),
                    &mut len,
                    None,
                    None,
                    None,
                    None,
                )
            };
            if status.is_err() {
                break;
            }
            let sub = PCWSTR(sub.as_ptr());
            let string = |value| reg_string(key, sub, value, query_view);
            let dword = |value| reg_dword(key, sub, value, query_view);
            if dword(w!("SystemComponent")) == 1 || string(w!("ParentKeyName")).is_some() {
                continue;
            }
            let Some(name) = string(w!("DisplayName")) else {
                continue;
            };
            let location = string(w!("InstallLocation"))
                .filter(|path| absolute_desktop_path(path))
                .map(|path| format!("{}\\", path.trim_end_matches('\\').to_lowercase()));
            let date = string(w!("InstallDate"))
                .and_then(|date| parse_date(&date, today))
                .map(|date| {
                    let label = if dword(w!("WindowsInstaller")) == 1 {
                        "Last serviced"
                    } else {
                        "Installer date"
                    };
                    (date, label)
                });
            entries.push(Registration {
                name: name.to_lowercase(),
                location,
                date,
            });
        }
        let _ = unsafe { RegCloseKey(key) };
    }
    entries
}

fn reg_string(key: HKEY, sub: PCWSTR, value: PCWSTR, view: REG_ROUTINE_FLAGS) -> Option<String> {
    let mut buffer = [0_u16; 1024];
    let mut len = size_of_val(&buffer) as u32;
    let status = unsafe {
        RegGetValueW(
            key,
            sub,
            value,
            RRF_RT_REG_SZ | view,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    if status.is_err() {
        return None;
    }
    let end = buffer.iter().position(|&unit| unit == 0)?;
    let text = String::from_utf16(&buffer[..end]).ok()?;
    (!text.trim().is_empty()).then(|| text.trim().to_string())
}

fn reg_dword(key: HKEY, sub: PCWSTR, value: PCWSTR, view: REG_ROUTINE_FLAGS) -> u32 {
    let mut number = 0_u32;
    let mut len = size_of_val(&number) as u32;
    let _ = unsafe {
        RegGetValueW(
            key,
            sub,
            value,
            RRF_RT_REG_DWORD | view,
            None,
            Some((&mut number as *mut u32).cast()),
            Some(&mut len),
        )
    };
    number
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str) -> AppEntry {
        AppEntry {
            name: id.into(),
            id: id.into(),
            path: None,
            desktop: None,
        }
    }

    #[test]
    fn baseline_has_no_dates_and_subsequent_apps_sort_newest_first() {
        let original = app("Original");
        let first = app("First");
        let second = app("Second");
        let mut seen = BTreeMap::new();
        assert!(record_seen(&mut seen, &[&original], 0));
        assert!(recent_rows(&[&original], &[], Some(&seen), 20261121).is_empty());
        assert!(record_seen(&mut seen, &[&original, &first], 20261118));
        assert!(record_seen(&mut seen, &[&first, &second], 20261120));
        assert!(!record_seen(&mut seen, &[&original, &first, &second], 20261121));
        let rows = recent_rows(&[&original, &first, &second], &[], Some(&seen), 20261121);
        assert_eq!(rows.iter().map(|row| row.app_index).collect::<Vec<_>>(), [2, 1]);
        assert_eq!(rows[0].date_label, "First seen · 2026-11-20");
        let serialized = toml::to_string(&seen).unwrap();
        assert_eq!(toml::from_str::<BTreeMap<String, u32>>(&serialized).unwrap(), seen);
    }

    #[test]
    fn installer_dates_reject_invalid_and_future_values() {
        assert_eq!(parse_date("20240229", 20261120), Some(20240229));
        for value in ["20260229", "20261121", "20260001", "2026-11-20", ""] {
            assert_eq!(parse_date(value, 20261120), None, "{value}");
        }
    }

    #[test]
    fn corroborated_path_beats_name_and_conflicting_dates_are_omitted() {
        let mut editor = app("Editor");
        let target = r"C:\Programs\Editor\editor.exe".to_string();
        editor.path = Some(target.clone());
        editor.desktop = Some(super::super::DesktopLaunch {
            target,
            arguments: None,
            directory: None,
        });
        let registration = |location: Option<&str>, date| Registration {
            name: "editor".into(),
            location: location.map(String::from),
            date: Some((date, "Last serviced")),
        };
        let location = Some("c:\\programs\\editor\\");
        let different = Some("c:\\programs\\editor beta\\");
        assert_eq!(
            registration_date(
                &editor,
                "editor",
                false,
                &[
                    registration(None, 20261120),
                    registration(different, 20261119),
                    registration(location, 20261118),
                ]
            ),
            Some((20261118, "Last serviced"))
        );
        assert_eq!(
            registration_date(
                &editor,
                "editor",
                true,
                &[registration(location, 20261120), registration(location, 20261119),]
            ),
            None
        );
        assert_eq!(
            registration_date(
                &editor,
                "editor",
                true,
                &[registration(different, 20261120), registration(None, 20261119),]
            ),
            None
        );
        assert_eq!(
            registration_date(&editor, "editor", false, &[registration(None, 20261119)]),
            None
        );
    }
}
