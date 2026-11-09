//! The mirrored notification-area icons, independent of Win32.

use super::wire::{self, Notify};
use windows::core::GUID;

/// How Explorer identifies an icon: by GUID when the app gave one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Guid(GUID),
    Window { hwnd: u32, uid: u32 },
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Key::Guid(guid) => write!(f, "{guid:?}"),
            Key::Window { hwnd, uid } => write!(f, "{hwnd:x}:{uid}"),
        }
    }
}

/// One app's icon. `I` is the owned image, so tests need no icons.
#[derive(Debug, Clone)]
pub struct Icon<I> {
    pub hwnd: u32,
    pub uid: u32,
    pub guid: Option<GUID>,
    /// The app's callback message; 0 until NIF_MESSAGE.
    pub callback: u32,
    /// Callback semantics from NIM_SETVERSION: 0, 3 or 4.
    pub version: u32,
    pub state: u32,
    pub tip: String,
    pub image: Option<I>,
    /// Changes with every update, so views can cache by it.
    pub revision: u64,
    pub pid: u32,
    /// Lowercase paths of the owner: its image path and shell32's encoded path.
    pub exes: Vec<String>,
    /// From NotifyIconSettings; `None` when Windows has no preference recorded.
    pub promoted: Option<bool>,
    /// The icon's place in Windows' order, from the same settings.
    pub order: Option<usize>,
    /// Explorer accepted the registration. A refresh's duplicate add is
    /// rejected although the icon is live, so it is still shown.
    pub accepted: bool,
}

impl<I> Icon<I> {
    pub fn key(&self) -> Key {
        match self.guid {
            Some(guid) => Key::Guid(guid),
            None => Key::Window {
                hwnd: self.hwnd,
                uid: self.uid,
            },
        }
    }

    /// Apps hide icons with NIS_HIDDEN; icons without an image have nothing to show.
    pub fn visible(&self) -> bool {
        self.image.is_some() && self.state & wire::NIS_HIDDEN == 0
    }
}

/// NIF_STATE only changes the bits in the mask.
pub fn merge_state(old: u32, new: u32, mask: u32) -> u32 {
    (old & !mask) | (new & mask)
}

/// The process that sent a packet.
#[derive(Debug, Clone, Default)]
pub struct Owner {
    pub pid: u32,
    pub exes: Vec<String>,
}

pub struct Model<I> {
    pub icons: Vec<Icon<I>>,
    revision: u64,
}

impl<I> Default for Model<I> {
    fn default() -> Self {
        Self {
            icons: Vec::new(),
            revision: 0,
        }
    }
}

impl<I> Model<I> {
    fn position(&self, guid: Option<GUID>, hwnd: u32, uid: u32) -> Option<usize> {
        self.icons.iter().position(|icon| match guid {
            Some(guid) => icon.guid == Some(guid),
            None => icon.guid.is_none() && icon.hwnd == hwnd && icon.uid == uid,
        })
    }

    /// Records a packet Explorer has answered. `image` is the copied icon for
    /// NIF_ICON: `Some(None)` clears it. Returns whether anything changed.
    pub fn apply(&mut self, n: &Notify, accepted: bool, image: Option<Option<I>>, owner: Owner) -> bool {
        let at = self.position(n.guid, n.hwnd, n.uid);
        self.revision += 1;
        let revision = self.revision;
        match (n.op, at) {
            (wire::NIM_DELETE, Some(at)) => {
                self.icons.remove(at);
                true
            }
            (wire::NIM_SETVERSION, Some(at)) if matches!(n.version, 0 | 3 | 4) => {
                let icon = &mut self.icons[at];
                icon.version = n.version;
                icon.revision = revision;
                true
            }
            (wire::NIM_ADD | wire::NIM_MODIFY, Some(at)) => {
                let icon = &mut self.icons[at];
                // A new registration starts with the default callback version.
                if n.op == wire::NIM_ADD && accepted {
                    icon.version = 0;
                    icon.accepted = true;
                }
                merge(icon, n, image, revision);
                if !owner.exes.is_empty() {
                    icon.pid = owner.pid;
                    icon.exes = owner.exes;
                }
                true
            }
            // A modify Explorer accepted is for an icon added before the proxy.
            (wire::NIM_ADD, None) | (wire::NIM_MODIFY, None) if n.op == wire::NIM_ADD || accepted => {
                let mut icon = Icon {
                    hwnd: n.hwnd,
                    uid: n.uid,
                    guid: n.guid,
                    callback: 0,
                    // For an existing Explorer icon the original SETVERSION
                    // may predate Wisp. Some clients also fill this field on
                    // registration updates (unless it holds a balloon timeout).
                    version: if (n.op == wire::NIM_MODIFY || !accepted)
                        && n.flags & (wire::NIF_MESSAGE | wire::NIF_INFO) == wire::NIF_MESSAGE
                        && matches!(n.version, 3 | 4)
                    {
                        n.version
                    } else {
                        0
                    },
                    state: 0,
                    tip: String::new(),
                    image: None,
                    revision,
                    pid: owner.pid,
                    exes: owner.exes,
                    promoted: None,
                    order: None,
                    accepted,
                };
                merge(&mut icon, n, image, revision);
                self.icons.push(icon);
                true
            }
            _ => false,
        }
    }

    /// Drops icons `alive` rejects, such as those of closed windows.
    pub fn retain(&mut self, alive: impl FnMut(&Icon<I>) -> bool) -> bool {
        let before = self.icons.len();
        self.icons.retain(alive);
        before != self.icons.len()
    }

    /// The icon a GetRect query names.
    pub fn find_query(&self, query: &wire::RectQuery) -> Option<&Icon<I>> {
        self.position(query.guid, query.hwnd, query.uid)
            .map(|at| &self.icons[at])
    }
}

fn merge<I>(icon: &mut Icon<I>, n: &Notify, image: Option<Option<I>>, revision: u64) {
    // GUID icons may move to another window; callbacks go to the latest.
    icon.hwnd = n.hwnd;
    icon.uid = n.uid;
    if n.flags & wire::NIF_MESSAGE != 0 {
        icon.callback = n.callback;
    }
    if let Some(image) = image {
        icon.image = image;
    }
    if let Some(tip) = &n.tip {
        icon.tip = tip.clone();
    }
    if n.flags & wire::NIF_STATE != 0 {
        icon.state = merge_state(icon.state, n.state, n.state_mask);
    }
    icon.revision = revision;
}

pub const WM_CONTEXTMENU: u32 = 0x007B;
pub const WM_MOUSEMOVE: u32 = 0x0200;
pub const WM_LBUTTONDOWN: u32 = 0x0201;
pub const WM_LBUTTONUP: u32 = 0x0202;
pub const WM_LBUTTONDBLCLK: u32 = 0x0203;
pub const WM_RBUTTONDOWN: u32 = 0x0204;
pub const WM_RBUTTONUP: u32 = 0x0205;
pub const NIN_SELECT: u32 = 0x0400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Click,
    /// The second click of a double click; the first was sent as a click.
    DoubleClick,
    Menu,
}

/// The events Explorer sends for a gesture: the mouse messages every app
/// understands, or the context-menu event for icons using the modern protocol.
pub fn events(action: Action, version: u32) -> Vec<u32> {
    let mut events = match action {
        Action::Click => vec![WM_MOUSEMOVE, WM_LBUTTONDOWN, WM_LBUTTONUP],
        Action::DoubleClick => vec![WM_LBUTTONDBLCLK, WM_LBUTTONUP],
        Action::Menu if version == 4 => vec![WM_CONTEXTMENU],
        Action::Menu => vec![WM_MOUSEMOVE, WM_RBUTTONDOWN, WM_RBUTTONUP],
    };
    if version >= 3 {
        match action {
            Action::Click => events.push(NIN_SELECT),
            Action::Menu | Action::DoubleClick => {}
        }
    }
    events
}

/// The callback message's wParam and lParam. Version 4 puts the anchor in
/// wParam and the event and 16-bit ID in lParam; earlier versions put the
/// ID in wParam and the event in lParam.
pub fn callback_words(version: u32, uid: u32, event: u32, x: i32, y: i32) -> (usize, isize) {
    if version == 4 {
        (
            wire::pack16(x, y) as usize,
            wire::pack16(event as i32, uid as i32) as isize,
        )
    } else {
        (uid as usize, event as isize)
    }
}

/// One `HKCU\Control Panel\NotifyIconSettings` entry.
#[derive(Debug, Clone, Default)]
pub struct Promotion {
    /// Lowercase: the stored path and its expanded form.
    pub exes: Vec<String>,
    pub uid: Option<u32>,
    pub guid: Option<GUID>,
    pub promoted: Option<bool>,
    /// Position in the key's UIOrderList.
    pub order: Option<usize>,
}

/// Matches by GUID, or by executable and ID. Entries without a recorded
/// preference lose to those with one, as stale entries pile up.
pub fn promotion<'a>(entries: &'a [Promotion], guid: Option<GUID>, exes: &[String], uid: u32) -> Option<&'a Promotion> {
    let matches = |entry: &&Promotion| match guid {
        Some(guid) => entry.guid == Some(guid),
        None => entry.guid.is_none() && entry.uid == Some(uid) && entry.exes.iter().any(|exe| exes.contains(exe)),
    };
    let mut found = entries.iter().filter(matches);
    found
        .clone()
        .find(|entry| entry.promoted.is_some())
        .or_else(|| found.next())
}

/// UIOrderList holds the settings' subkey names as little-endian u64s.
pub fn order_list(bytes: &[u8]) -> Vec<u64> {
    bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| u64::from_le_bytes(*c))
        .collect()
}

/// Lowercase, without the long-path prefix or quotes.
pub fn normalize_exe(path: &str) -> String {
    path.trim()
        .trim_matches('"')
        .trim_start_matches(r"\\?\")
        .replace('/', "\\")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tray_host::wire::tests::packet;
    use crate::tray_host::wire::*;

    fn parse(op: u32, flags: u32, uid: u32, tip: &str) -> Notify {
        parse_notify(&packet(op, flags, 0x100, uid, tip)).unwrap()
    }

    fn owner() -> Owner {
        Owner {
            pid: 42,
            exes: vec![r"c:\a.exe".into()],
        }
    }

    #[test]
    fn state_mask_changes_only_masked_bits() {
        assert_eq!(merge_state(0b1010, 0b0101, 0b0011), 0b1001);
        assert_eq!(merge_state(NIS_HIDDEN, 0, 0), NIS_HIDDEN);
        assert_eq!(merge_state(NIS_HIDDEN, 0, NIS_HIDDEN), 0);
        assert_eq!(merge_state(0, NIS_HIDDEN, NIS_HIDDEN), NIS_HIDDEN);
    }

    #[test]
    fn add_modify_version_delete() {
        let mut model = Model::<u8>::default();
        let mut add = parse(NIM_ADD, NIF_MESSAGE | NIF_ICON | NIF_TIP, 1, "One");
        add.state_mask = 0;
        assert!(model.apply(&add, true, Some(Some(1)), owner()));
        let icon = &model.icons[0];
        assert_eq!(
            (icon.callback, icon.tip.as_str(), icon.image, icon.version),
            (0x8001, "One", Some(1), 0)
        );
        assert!(icon.visible() && icon.accepted);
        let first = icon.revision;

        let set = parse(NIM_SETVERSION, 0, 1, "");
        assert!(model.apply(&set, true, None, Owner::default()));
        assert_eq!(model.icons[0].version, 4);
        assert!(model.icons[0].revision > first);
        // Explorer rejects versions 1 and 2; so does the model.
        let mut bad = set.clone();
        bad.version = 2;
        assert!(!model.apply(&bad, false, None, Owner::default()));
        assert_eq!(model.icons[0].version, 4);

        // A modify without NIF_TIP keeps the tip; NIF_STATE hides it.
        let modify = parse(NIM_MODIFY, NIF_ICON | NIF_STATE, 1, "ignored");
        assert!(model.apply(&modify, true, Some(Some(2)), Owner::default()));
        let icon = &model.icons[0];
        assert_eq!((icon.tip.as_str(), icon.image, icon.pid), ("One", Some(2), 42));
        assert!(!icon.visible());

        // A refresh's duplicate add keeps the negotiated version.
        assert!(model.apply(&add, false, Some(Some(3)), owner()));
        assert_eq!((model.icons[0].version, model.icons.len()), (4, 1));
        assert!(model.apply(&add, true, Some(Some(3)), owner()));
        assert_eq!(model.icons[0].version, 0);

        assert!(model.apply(&parse(NIM_DELETE, 0, 1, ""), true, None, Owner::default()));
        assert!(model.icons.is_empty());
        assert!(!model.apply(&parse(NIM_DELETE, 0, 1, ""), false, None, Owner::default()));
    }

    #[test]
    fn unknown_icons_come_from_adds_or_accepted_modifies() {
        let mut model = Model::<u8>::default();
        // Explorer already has icons added before the proxy, so their
        // refresh adds fail but they are live.
        assert!(model.apply(&parse(NIM_ADD, NIF_ICON, 1, ""), false, Some(Some(1)), owner()));
        assert!(!model.apply(&parse(NIM_MODIFY, NIF_ICON, 2, ""), false, Some(Some(1)), owner()));
        assert!(model.apply(&parse(NIM_MODIFY, NIF_ICON, 3, ""), true, Some(Some(1)), owner()));
        assert_eq!(model.icons.iter().map(|i| i.uid).collect::<Vec<_>>(), [1, 3]);
        assert!(!model.apply(&parse(NIM_SETVERSION, 0, 9, ""), true, None, owner()));
        assert!(model.retain(|icon| icon.uid != 1));
        assert!(!model.retain(|_| true));
        assert_eq!(model.icons.len(), 1);
    }

    #[test]
    fn guid_icons_are_keyed_by_guid() {
        let mut model = Model::<u8>::default();
        let guid = GUID::from_u128(0x1234);
        let mut add = parse(NIM_ADD, NIF_ICON | NIF_GUID, 1, "");
        add.guid = Some(guid);
        model.apply(&add, true, Some(Some(1)), owner());
        let mut moved = parse(NIM_MODIFY, NIF_TIP | NIF_GUID, 9, "Moved");
        moved.guid = Some(guid);
        moved.hwnd = 0x200;
        model.apply(&moved, true, None, owner());
        assert_eq!(model.icons.len(), 1);
        assert_eq!((model.icons[0].hwnd, model.icons[0].tip.as_str()), (0x200, "Moved"));
        assert_eq!(model.icons[0].key(), Key::Guid(guid));
        // The same window and ID without the GUID is another icon.
        model.apply(&parse(NIM_ADD, NIF_ICON, 9, ""), true, Some(Some(1)), owner());
        assert_eq!(model.icons.len(), 2);
        let query = RectQuery {
            query: 1,
            hwnd: 0,
            uid: 0,
            guid: Some(guid),
        };
        assert_eq!(model.find_query(&query).unwrap().tip, "Moved");
    }

    #[test]
    fn callback_words_follow_the_version() {
        // Version 4: anchor in wParam, event and ID in lParam.
        let (wp, lp) = callback_words(4, 7, NIN_SELECT, 1500, 1170);
        assert_eq!(wp, 1500 | (1170 << 16));
        assert_eq!(lp, (NIN_SELECT | (7 << 16)) as isize);
        // Negative coordinates on a monitor left of or above the primary.
        let (wp, _) = callback_words(4, 7, WM_CONTEXTMENU, -10, -1);
        assert_eq!(((wp & 0xFFFF) as u16 as i16, (wp >> 16) as u16 as i16), (-10, -1));
        // Only the low 16 bits of the ID fit.
        let (_, lp) = callback_words(4, 0x1_0002, WM_LBUTTONUP, 0, 0);
        assert_eq!(lp >> 16, 2);
        // Versions 0 and 3: the full ID and the bare event.
        for version in [0, 3] {
            assert_eq!(
                callback_words(version, 0x1_0002, WM_LBUTTONUP, 5, 5),
                (0x1_0002, WM_LBUTTONUP as isize)
            );
        }
    }

    #[test]
    fn existing_icons_can_recover_the_callback_version() {
        let mut model = Model::<()>::default();
        for (uid, accepted, extra, version) in [(1, false, 0, 4), (2, true, 0, 0), (3, false, NIF_INFO, 0)] {
            let add = parse(NIM_ADD, NIF_MESSAGE | extra, uid, "");
            assert!(model.apply(&add, accepted, None, owner()));
            assert_eq!(model.icons.last().unwrap().version, version);
        }
    }

    #[test]
    fn events_add_selection_for_new_versions() {
        assert_eq!(events(Action::Click, 0), [WM_MOUSEMOVE, WM_LBUTTONDOWN, WM_LBUTTONUP]);
        assert_eq!(events(Action::Click, 4).last(), Some(&NIN_SELECT));
        assert_eq!(events(Action::Menu, 3), [WM_MOUSEMOVE, WM_RBUTTONDOWN, WM_RBUTTONUP]);
        assert_eq!(events(Action::Menu, 4), [WM_CONTEXTMENU]);
        assert_eq!(events(Action::Menu, 0).last(), Some(&WM_RBUTTONUP));
        assert_eq!(events(Action::DoubleClick, 4), [WM_LBUTTONDBLCLK, WM_LBUTTONUP]);
    }

    #[test]
    fn promotion_matches_guid_or_exe_and_id() {
        let guid = GUID::from_u128(0x99);
        let entry = |exes: &[&str], uid, guid, promoted, order| Promotion {
            exes: exes.iter().map(|s| s.to_string()).collect(),
            uid,
            guid,
            promoted,
            order,
        };
        let entries = vec![
            entry(&[r"c:\stale\a.exe"], Some(1), None, Some(false), None),
            entry(&[r"{guid}\a.exe", r"c:\a.exe"], Some(1), None, None, Some(0)),
            entry(&[r"{guid}\a.exe", r"c:\a.exe"], Some(1), None, Some(true), Some(1)),
            entry(&[r"c:\b.exe"], None, Some(guid), Some(false), None),
            entry(&[r"c:\c.exe"], Some(5), None, None, Some(4)),
        ];
        let exes = vec![r"c:\a.exe".to_string()];
        let find = |guid, exes: &[String], uid| promotion(&entries, guid, exes, uid).map(|e| (e.promoted, e.order));
        assert_eq!(find(None, &exes, 1), Some((Some(true), Some(1))));
        assert_eq!(find(None, &exes, 2), None);
        assert_eq!(find(Some(guid), &[], 0), Some((Some(false), None)));
        assert_eq!(find(Some(GUID::from_u128(1)), &exes, 1), None);
        assert_eq!(find(None, &[r"c:\c.exe".into()], 5), Some((None, Some(4))));
        assert_eq!(order_list(&[1, 0, 0, 0, 0, 0, 0, 0x80, 9]), [0x8000_0000_0000_0001]);
        assert_eq!(normalize_exe(r#""\\?\C:/Apps/A.exe""#), r"c:\apps\a.exe");
    }
}
