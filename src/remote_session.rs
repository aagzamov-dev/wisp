//! WTS notifications belong to the launcher HWND; no polling or session mutation.
use std::ffi::c_void;
use windows::Win32::Foundation::HWND;

pub const WM_WTSSESSION_CHANGE: u32 = 0x02B1;
#[link(name = "wtsapi32")]
unsafe extern "system" {
    fn WTSRegisterSessionNotification(hwnd: *mut c_void, flags: u32) -> i32;
    fn WTSUnRegisterSessionNotification(hwnd: *mut c_void) -> i32;
    fn WTSQuerySessionInformationW(
        server: *mut c_void,
        session: u32,
        class: i32,
        buffer: *mut *mut c_void,
        bytes: *mut u32,
    ) -> i32;
    fn WTSFreeMemory(memory: *mut c_void);
}
struct Buffer(*mut c_void);
impl Drop for Buffer {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                WTSFreeMemory(self.0);
            }
        }
    }
}
fn query<T: Copy>(class: i32) -> std::io::Result<T> {
    let mut buffer = Buffer(std::ptr::null_mut());
    let mut bytes = 0;
    // WTS_CURRENT_SESSION; SDK classes WTSSessionId=4, WTSClientProtocolType=16.
    let ok = unsafe { WTSQuerySessionInformationW(std::ptr::null_mut(), u32::MAX, class, &mut buffer.0, &mut bytes) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if buffer.0.is_null() || bytes < std::mem::size_of::<T>() as u32 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Invalid WTS response",
        ));
    }
    Ok(unsafe { buffer.0.cast::<T>().read_unaligned() })
}
pub fn protocol() -> std::io::Result<bool> {
    query::<u16>(16).map(|protocol| protocol == 2)
}

pub struct Notifications {
    hwnd: HWND,
    pub session: Option<u32>,
    registered: bool,
}
impl Notifications {
    pub fn new(hwnd: HWND) -> Self {
        let session = match query::<u32>(4) {
            Ok(id) => Some(id),
            Err(error) => {
                crate::logging::error("remote.session_id", format_args!("error={error}"));
                None
            }
        };
        let registered = unsafe { WTSRegisterSessionNotification(hwnd.0, 0) } != 0;
        if !registered {
            crate::logging::error(
                "remote.register",
                format_args!("error={}", std::io::Error::last_os_error()),
            );
        }
        Self {
            hwnd,
            session,
            registered,
        }
    }
}
impl Drop for Notifications {
    fn drop(&mut self) {
        if self.registered && unsafe { WTSUnRegisterSessionNotification(self.hwnd.0) } == 0 {
            crate::logging::error(
                "remote.unregister",
                format_args!("error={}", std::io::Error::last_os_error()),
            );
        }
    }
}
/// Disconnects trigger a fresh protocol query, never an assumed console switch.
pub fn relevant(current: Option<u32>, event_session: u32, event: usize) -> bool {
    current.is_none_or(|session| session == event_session) && matches!(event, 1..=9 | 0xF)
}
/// Unknown/failing observations preserve the last known layout (normal at startup).
pub fn observed(previous: bool, query: std::io::Result<bool>) -> bool {
    query.unwrap_or(previous)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reconnect_and_failure_preserve_authoritative_session_state() {
        let mut rdp = observed(false, Ok(true));
        assert!(rdp);
        assert!(!relevant(Some(12), 13, 1));
        assert!(relevant(Some(12), 12, 4));
        rdp = observed(rdp, Err(std::io::ErrorKind::Other.into()));
        assert!(rdp);
        rdp = observed(rdp, Ok(true));
        assert!(rdp); // disconnected RDP remains RDP
        rdp = observed(rdp, Ok(false));
        assert!(!rdp); // actual console return
        assert!(!observed(false, Err(std::io::ErrorKind::Other.into())));
        assert!(relevant(None, 12, 1));
    }
}
