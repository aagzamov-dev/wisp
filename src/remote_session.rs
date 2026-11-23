//! Session notifications and the policy for reclaiming the physical desktop.
use std::ffi::c_void;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_REMOTESESSION};

pub const WM_WTSSESSION_CHANGE: u32 = 0x02B1;
const WTS_CURRENT_SESSION: u32 = u32::MAX;
const WTS_CONNECT_STATE: i32 = 8;
const WTS_ACTIVE: u32 = 0;
const WTS_CLIENT_PROTOCOL_TYPE: i32 = 16;
const WTS_REMOTE_CONNECT: usize = 3;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn WTSGetActiveConsoleSessionId() -> u32;
    fn ProcessIdToSessionId(process_id: u32, session_id: *mut u32) -> i32;
}

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
            unsafe { WTSFreeMemory(self.0) };
        }
    }
}

fn query<T: Copy>(class: i32) -> std::io::Result<T> {
    let mut buffer = Buffer(std::ptr::null_mut());
    let mut bytes = 0;
    let ok = unsafe {
        WTSQuerySessionInformationW(
            std::ptr::null_mut(),
            WTS_CURRENT_SESSION,
            class,
            &mut buffer.0,
            &mut bytes,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if buffer.0.is_null() || bytes < size_of::<T>() as u32 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Invalid WTS response",
        ));
    }
    Ok(unsafe { buffer.0.cast::<T>().read_unaligned() })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Session {
    Unknown,
    Console,
    Remote,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observation {
    Confirmed(Session),
    ProvisionalConsole,
    Unknown,
}

impl Observation {
    fn from_snapshot(protocol: Option<u16>, own_console: bool, active: Option<bool>, local_metric: bool) -> Self {
        match (protocol, own_console, active) {
            (Some(2), _, _) => Self::Confirmed(Session::Remote),
            (Some(0), true, Some(true)) => Self::Confirmed(Session::Console),
            (None | Some(0), true, None | Some(true)) if local_metric => Self::ProvisionalConsole,
            _ => Self::Unknown,
        }
    }

    pub fn session(self, previous: Session) -> Session {
        match self {
            Self::Confirmed(session) => session,
            // A native startup fallback can allow local work while WTS
            // initializes. It never overrides a known remote connection.
            Self::ProvisionalConsole if previous == Session::Unknown => Session::Console,
            _ => previous,
        }
    }

    pub fn settled(self, waiting_for_console: bool) -> bool {
        self == Self::Confirmed(Session::Console) || (!waiting_for_console && self == Self::Confirmed(Session::Remote))
    }
}

pub fn observe(session_id: u32) -> Observation {
    let protocol = query::<u16>(WTS_CLIENT_PROTOCOL_TYPE)
        .inspect_err(|error| crate::logging::error("remote.query_protocol", format_args!("error={error}")))
        .ok();
    let console = owns_console(session_id);
    if !console || protocol.is_some_and(|protocol| protocol != 0) {
        return Observation::from_snapshot(protocol, console, None, false);
    }
    let active = query::<u32>(WTS_CONNECT_STATE)
        .inspect_err(|error| crate::logging::error("remote.query_state", format_args!("error={error}")))
        .ok()
        .map(|state| state == WTS_ACTIVE);
    let local_metric = (protocol.is_none() || active.is_none()) && unsafe { GetSystemMetrics(SM_REMOTESESSION) } == 0;
    Observation::from_snapshot(protocol, console, active, local_metric)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Ignore,
    RemoteConnect,
    ConsoleConnect,
    ConsoleDisconnect,
    SessionReady,
}

pub fn event_kind(session: u32, event_session: u32, event: usize) -> Event {
    if session != event_session {
        return Event::Ignore;
    }
    match event {
        WTS_REMOTE_CONNECT => Event::RemoteConnect,
        1 => Event::ConsoleConnect,
        2 => Event::ConsoleDisconnect,
        // Logon, unlock and desktop-ready also occur in healthy RDP sessions.
        5 | 8 | 0xF => Event::SessionReady,
        _ => Event::Ignore,
    }
}

pub fn owns_console(session_id: u32) -> bool {
    (unsafe { WTSGetActiveConsoleSessionId() }) == session_id
}

pub fn current_session_id() -> std::io::Result<u32> {
    let mut id = u32::MAX;
    if unsafe { ProcessIdToSessionId(std::process::id(), &mut id) } == 0 || id == u32::MAX {
        return Err(std::io::Error::last_os_error());
    }
    Ok(id)
}

pub struct Notifications(HWND);

impl Notifications {
    pub fn new(hwnd: HWND) -> std::io::Result<Self> {
        // NOTIFY_FOR_THIS_SESSION limits the messages Windows sends to this HWND.
        if unsafe { WTSRegisterSessionNotification(hwnd.0, 0) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self(hwnd))
    }
}

impl Drop for Notifications {
    fn drop(&mut self) {
        if unsafe { WTSUnRegisterSessionNotification(self.0.0) } == 0 {
            crate::logging::error(
                "remote.unregister",
                format_args!("error={}", std::io::Error::last_os_error()),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_current_physical_console_can_resume_wisp() {
        let remote = Observation::from_snapshot(Some(2), false, None, false);
        let detached = Observation::from_snapshot(Some(0), false, None, false);
        let stale_console = Observation::from_snapshot(None, true, Some(true), true);
        let console = Observation::from_snapshot(Some(0), true, Some(true), false);
        let mut session = remote.session(Session::Unknown);
        assert_eq!(session, Session::Remote);
        session = detached.session(session);
        assert_eq!(session, Session::Remote);
        session = stale_console.session(session);
        assert_eq!(session, Session::Remote);
        assert!(!stale_console.settled(true));
        assert_eq!(console.session(session), Session::Console);
        assert!(console.settled(true));
    }

    #[test]
    fn an_unknown_startup_never_takes_shell_ownership() {
        assert_eq!(
            Observation::from_snapshot(None, true, None, false).session(Session::Unknown),
            Session::Unknown
        );
        assert_eq!(
            Observation::from_snapshot(Some(0), false, None, false).session(Session::Unknown),
            Session::Unknown
        );
    }

    #[test]
    fn registration_recovery_retries_after_a_failed_query_with_old_console_state() {
        let provisional = Observation::from_snapshot(None, true, None, true);
        let mut session = provisional.session(Session::Unknown);
        assert_eq!(session, Session::Console);
        assert!(!provisional.settled(false));

        // WTS registration succeeds after an RDP connection, but the protocol
        // query still fails. Preserve the current policy and keep observing.
        let failed = Observation::from_snapshot(None, false, None, false);
        session = failed.session(session);
        assert_eq!(session, Session::Console);
        assert!(!failed.settled(false));

        let confirmed = Observation::from_snapshot(Some(2), false, None, false);
        assert_eq!(confirmed.session(session), Session::Remote);
        assert!(confirmed.settled(false));
    }

    #[test]
    fn only_own_session_notifications_affect_wisp() {
        assert_eq!(event_kind(12, 13, WTS_REMOTE_CONNECT), Event::Ignore);
        assert_eq!(event_kind(12, 12, WTS_REMOTE_CONNECT), Event::RemoteConnect);
        assert_eq!(event_kind(12, 12, 4), Event::Ignore);
        assert_eq!(event_kind(12, 12, 1), Event::ConsoleConnect);
        assert_eq!(event_kind(12, 12, 7), Event::Ignore);
    }
}
