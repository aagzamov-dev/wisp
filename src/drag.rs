//! Shell drag data and the system drag threshold.

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{BITMAP, DeleteObject, GetObjectW};
use windows::Win32::System::Com::*;
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Ole::*;
use windows::Win32::UI::Shell::*;
use windows::core::{HSTRING, Result, w};

use crate::search::{Action, Kind, ResultItem};

pub struct Ole;

impl Ole {
    pub fn new() -> Result<Self> {
        unsafe { OleInitialize(None)? };
        Ok(Self)
    }
}

impl Drop for Ole {
    fn drop(&mut self) {
        unsafe { OleUninitialize() };
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Payload {
    File(String),
    App(String),
    Text(String),
    Url(String),
}

impl Payload {
    pub fn from_result(item: &ResultItem) -> Option<Self> {
        match (&item.kind, &item.action) {
            (Kind::File | Kind::Folder, Action::Open(path)) => Some(Self::File(path.clone())),
            (
                Kind::App,
                Action::Launch {
                    path: Some(path),
                    admin: false,
                    ..
                },
            ) => Some(Self::App(path.clone())),
            (_, Action::Copy(text)) => Some(Self::Text(text.clone())),
            (Kind::Url, Action::Open(url)) => Some(Self::Url(url.clone())),
            _ => None,
        }
    }

    pub fn effect(&self) -> DROPEFFECT {
        match self {
            Self::App(_) => DROPEFFECT_LINK,
            Self::File(_) => DROPEFFECT_COPY | DROPEFFECT_LINK,
            Self::Text(_) | Self::Url(_) => DROPEFFECT_COPY,
        }
    }

    pub fn set_image(&self, data: &IDataObject) -> Result<()> {
        let (Self::File(path) | Self::App(path)) = self else {
            return Ok(());
        };
        unsafe {
            let factory: IShellItemImageFactory = SHCreateItemFromParsingName(&HSTRING::from(path), None)?;
            let helper: IDragSourceHelper = CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER)?;
            let bitmap = factory.GetImage(SIZE { cx: 48, cy: 48 }, SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK)?;
            let mut info = BITMAP::default();
            GetObjectW(
                bitmap.into(),
                size_of::<BITMAP>() as i32,
                Some((&mut info as *mut BITMAP).cast()),
            );
            let image = SHDRAGIMAGE {
                sizeDragImage: SIZE {
                    cx: info.bmWidth,
                    cy: info.bmHeight,
                },
                ptOffset: POINT {
                    x: info.bmWidth / 2,
                    y: info.bmHeight / 2,
                },
                hbmpDragImage: bitmap,
                crColorKey: COLORREF(0xFFFFFFFF),
            };
            let result = helper.InitializeFromBitmap(&image, data);
            let _ = DeleteObject(bitmap.into());
            result
        }
    }

    pub fn data_object(&self) -> Result<IDataObject> {
        unsafe {
            match self {
                Self::File(path) | Self::App(path) => {
                    let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(path), None)?;
                    item.BindToHandler(None, &BHID_DataObject)
                }
                Self::Text(text) | Self::Url(text) => {
                    let data: IDataObject = SHCreateDataObject(None, None, None)?;
                    set_text(&data, 13, text)?; // CF_UNICODETEXT
                    if matches!(self, Self::Url(_)) {
                        let format = RegisterClipboardFormatW(w!("UniformResourceLocatorW"));
                        if format == 0 {
                            return Err(windows::core::Error::from_thread());
                        }
                        set_text(&data, format as u16, text)?;
                    }
                    Ok(data)
                }
            }
        }
    }
}

fn set_text(data: &IDataObject, format: u16, text: &str) -> Result<()> {
    let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let mem = GlobalAlloc(GMEM_MOVEABLE, text.len() * 2)?;
        let ptr = GlobalLock(mem).cast::<u16>();
        if ptr.is_null() {
            let error = windows::core::Error::from_thread();
            let _ = GlobalFree(Some(mem));
            return Err(error);
        }
        std::ptr::copy_nonoverlapping(text.as_ptr(), ptr, text.len());
        let _ = GlobalUnlock(mem);
        let mut medium = STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: mem },
            ..Default::default()
        };
        let result = data.SetData(&format_etc(format), &medium, true);
        if result.is_err() {
            ReleaseStgMedium(&mut medium);
        }
        result
    }
}

fn format_etc(format: u16) -> FORMATETC {
    FORMATETC {
        cfFormat: format,
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
        ..Default::default()
    }
}

pub fn crossed_threshold(start: (i32, i32), now: (i32, i32), size: (i32, i32)) -> bool {
    // SM_CXDRAG/SM_CYDRAG give the full rectangle centered on the press.
    (now.0 as i64 - start.0 as i64).abs() > (size.0 / 2) as i64
        || (now.1 as i64 - start.1 as i64).abs() > (size.1 / 2) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn threshold_uses_system_rectangle() {
        assert!(!crossed_threshold((10, 20), (12, 18), (4, 4)));
        assert!(crossed_threshold((10, 20), (13, 20), (4, 4)));
        assert!(crossed_threshold((10, 20), (10, 17), (4, 4)));
        assert!(crossed_threshold((i32::MIN, 0), (i32::MAX, 0), (4, 4)));
    }

    #[test]
    fn payloads_follow_actions() {
        let item = |kind, action| ResultItem::new(kind, "title", "subtitle", action);
        for kind in [Kind::File, Kind::Folder] {
            assert_eq!(
                Payload::from_result(&item(kind, Action::Open("path".into()))),
                Some(Payload::File("path".into()))
            );
        }
        for kind in [Kind::Calc, Kind::Command, Kind::File] {
            assert_eq!(
                Payload::from_result(&item(kind, Action::Copy("value".into()))),
                Some(Payload::Text("value".into()))
            );
        }
        assert_eq!(
            Payload::from_result(&item(Kind::Url, Action::Open("https://example.com/".into()))),
            Some(Payload::Url("https://example.com/".into()))
        );
        let app = |path, admin| {
            item(
                Kind::App,
                Action::Launch {
                    id: "id".into(),
                    path,
                    admin,
                },
            )
        };
        assert_eq!(
            Payload::from_result(&app(Some("app.exe".into()), false)),
            Some(Payload::App("app.exe".into()))
        );
        assert!(Payload::from_result(&app(None, false)).is_none());
        assert!(Payload::from_result(&app(Some("app.exe".into()), true)).is_none());
        assert!(
            Payload::from_result(&item(
                Kind::Shell,
                Action::Shell {
                    command: "echo hello".into(),
                    admin: false
                }
            ))
            .is_none()
        );
        assert!(Payload::from_result(&item(Kind::File, Action::Reveal("path".into()))).is_none());
        assert!(Payload::from_result(&item(Kind::Command, Action::None)).is_none());
        assert_eq!(Payload::App(String::new()).effect(), DROPEFFECT_LINK);
        assert_eq!(Payload::File(String::new()).effect(), DROPEFFECT_COPY | DROPEFFECT_LINK);
    }

    struct Medium(STGMEDIUM);
    impl Drop for Medium {
        fn drop(&mut self) {
            unsafe {
                ReleaseStgMedium(&mut self.0);
            }
        }
    }

    fn get(data: &IDataObject, format: u16) -> Medium {
        unsafe {
            assert!(data.QueryGetData(&format_etc(format)).is_ok());
            Medium(data.GetData(&format_etc(format)).unwrap())
        }
    }

    fn read_text(data: &IDataObject, format: u16) -> String {
        let medium = get(data, format);
        unsafe {
            let mem = medium.0.u.hGlobal;
            let ptr = GlobalLock(mem).cast::<u16>();
            assert!(!ptr.is_null());
            let units = std::slice::from_raw_parts(ptr, GlobalSize(mem) / 2);
            let len = units.iter().position(|&unit| unit == 0).unwrap();
            let text = String::from_utf16(&units[..len]).unwrap();
            let _ = GlobalUnlock(mem);
            text
        }
    }

    #[test]
    fn text_and_url_formats_round_trip() {
        let _ole = Ole::new().unwrap();
        for value in ["14.142135", "Tokyo 12:30", "C:\\\u{8cc7}\u{6599}\\invoice.pdf", ""] {
            let data = Payload::Text(value.into()).data_object().unwrap();
            assert_eq!(read_text(&data, 13), value);
        }
        let url = "https://example.com/search?q=invoice%20paid";
        let data = Payload::Url(url.into()).data_object().unwrap();
        assert_eq!(read_text(&data, 13), url);
        let format = unsafe { RegisterClipboardFormatW(w!("UniformResourceLocatorW")) } as u16;
        assert_eq!(read_text(&data, format), url);
    }

    #[test]
    fn shell_formats_round_trip_files_folders_and_apps() {
        let _ole = Ole::new().unwrap();
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "wisp-drag-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        // The shell reports long paths; %TEMP% can hold an 8.3 name such as RUNNER~1.
        let dir = std::fs::canonicalize(&dir).unwrap();
        let dir = std::path::PathBuf::from(dir.to_str().unwrap().trim_start_matches(r"\\?\"));
        let file = dir.join("invoice \u{8cc7}\u{6599}.txt");
        std::fs::write(&file, "invoice").unwrap();
        let shortcut = dir.join("App.lnk");
        unsafe {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
            link.SetPath(&HSTRING::from(std::env::current_exe().unwrap().to_str().unwrap()))
                .unwrap();
            let persist: IPersistFile = windows::core::Interface::cast(&link).unwrap();
            persist.Save(&HSTRING::from(shortcut.to_str().unwrap()), true).unwrap();
        }
        for payload in [
            Payload::App(shortcut.to_str().unwrap().into()),
            Payload::File(file.to_str().unwrap().into()),
            Payload::File(dir.to_str().unwrap().into()),
            Payload::App(std::env::current_exe().unwrap().to_str().unwrap().into()),
        ] {
            let data = payload.data_object().unwrap();
            payload.set_image(&data).unwrap();
            let medium = get(&data, 15); // CF_HDROP
            unsafe {
                let drop = HDROP(medium.0.u.hGlobal.0);
                assert_eq!(DragQueryFileW(drop, u32::MAX, None), 1);
                let len = DragQueryFileW(drop, 0, None);
                let mut path = vec![0; len as usize + 1];
                assert_eq!(DragQueryFileW(drop, 0, Some(&mut path)), len);
                let expected = match payload {
                    Payload::File(path) | Payload::App(path) => path,
                    _ => unreachable!(),
                };
                assert_eq!(String::from_utf16(&path[..len as usize]).unwrap(), expected);
                let shell_id = RegisterClipboardFormatW(w!("Shell IDList Array")) as u16;
                let _ = get(&data, shell_id);
            }
        }
        std::fs::remove_file(shortcut).unwrap();
        std::fs::remove_file(file).unwrap();
        std::fs::remove_dir(dir).unwrap();
        assert!(
            Payload::File("Z:\\wisp-nonexistent-file-9d238c.txt".into())
                .data_object()
                .is_err()
        );
    }
}
