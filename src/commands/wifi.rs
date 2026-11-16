//! Wi-Fi: radio state, the connected network, nearby or saved networks, and
//! connecting to a saved network.
//!
//! Since Windows 11 24H2, nearby networks and the connected network's name
//! need location access. Without it, Wisp lists saved networks and reads the
//! connected network's name from the Network List Manager.

use windows::{
    Devices::Radios::RadioKind,
    Win32::{
        Foundation::{ERROR_ACCESS_DENIED, HANDLE},
        NetworkManagement::WiFi::*,
        Networking::NetworkListManager::{INetworkConnection, INetworkListManager, NetworkListManager},
        System::Com::{CLSCTX_ALL, CoCreateInstance},
    },
    core::{GUID, HSTRING, PCWSTR},
};

use super::radio;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

/// Menu discovery is separate from the launcher's inexpensive cached lookup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ScanStatus {
    #[default]
    Idle,
    Scanning,
    Ready,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct State {
    pub on: bool,
    /// The connected network's name.
    pub network: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Network {
    pub name: String,
    /// The saved profile, used to connect.
    pub profile: Option<String>,
    pub connected: bool,
    pub secured: bool,
    /// Signal quality from 0 to 100. Saved networks listed without location
    /// access have no signal.
    pub signal: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Networks {
    pub list: Vec<Network>,
    /// Windows denied the list of nearby networks, so `list` holds saved networks.
    pub location_off: bool,
}

/// `None` if the PC has no Wi-Fi adapter.
pub fn state() -> Option<State> {
    let on = radio::is_on(RadioKind::WiFi)?;
    Some(State {
        on,
        network: if on { connected_name() } else { None },
    })
}

pub fn set_power(on: bool) -> Result<(), String> {
    radio::set(RadioKind::WiFi, "Wi-Fi", on)
}

/// Lists nearby networks, or saved networks if location access is off.
pub fn networks() -> Result<Networks, String> {
    let client = Client::open()?;
    let interface = client.interface().ok_or("This PC has no Wi-Fi adapter.")?;
    read_networks(&client, &interface)
}

fn read_networks(client: &Client, interface: &GUID) -> Result<Networks, String> {
    match client.available(interface) {
        Ok(list) => Ok(Networks {
            list,
            location_off: false,
        }),
        Err(code) if code == ERROR_ACCESS_DENIED.0 => {
            let connected = connection_name(client, interface);
            Ok(saved_networks(client, interface, connected.as_deref()))
        }
        Err(code) => Err(windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(code)).message()),
    }
}

fn saved_networks(client: &Client, interface: &GUID, connected: Option<&str>) -> Networks {
    let list = client
        .profiles(interface)
        .into_iter()
        .map(|name| Network {
            connected: connected == Some(name.as_str()),
            profile: Some(name.clone()),
            name,
            secured: true,
            signal: None,
        })
        .collect();
    Networks {
        list: sorted(list),
        location_off: true,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScanSignal {
    Complete,
    Failed,
}

fn scan_signal(interface: &GUID, data: &L2_NOTIFICATION_DATA) -> Option<ScanSignal> {
    if data.InterfaceGuid != *interface
        || data.NotificationSource & WLAN_NOTIFICATION_SOURCE_ACM == WLAN_NOTIFICATION_SOURCES(0)
    {
        return None;
    }
    if data.NotificationCode == wlan_notification_acm_scan_complete.0 as u32 {
        Some(ScanSignal::Complete)
    } else if data.NotificationCode == wlan_notification_acm_scan_fail.0 as u32 {
        Some(ScanSignal::Failed)
    } else {
        None
    }
}

struct ScanContext {
    interface: GUID,
    signals: mpsc::SyncSender<ScanSignal>,
    armed: AtomicBool,
}
unsafe extern "system" fn scan_notification(data: *mut L2_NOTIFICATION_DATA, context: *mut core::ffi::c_void) {
    // No blocking work, shared locks, WLAN calls or pointer retention in the callback.
    if let (Some(data), Some(context)) = (unsafe { data.as_ref() }, unsafe {
        context.cast::<ScanContext>().as_ref()
    }) && context.armed.load(Ordering::Acquire)
        && let Some(signal) = scan_signal(&context.interface, data)
    {
        let _ = context.signals.try_send(signal);
    }
}

struct ScanSession {
    client: Option<Client>,
    context: Box<ScanContext>,
    signals: mpsc::Receiver<ScanSignal>,
}
impl ScanSession {
    fn new(client: Client, interface: GUID) -> Result<Self, String> {
        let (sender, signals) = mpsc::sync_channel(1);
        let session = Self {
            client: Some(client),
            context: Box::new(ScanContext {
                interface,
                signals: sender,
                armed: AtomicBool::new(false),
            }),
            signals,
        };
        let code = unsafe {
            WlanRegisterNotification(
                session.handle(),
                WLAN_NOTIFICATION_SOURCE_ACM,
                false,
                Some(scan_notification),
                Some((&raw const *session.context).cast()),
                None,
                None,
            )
        };
        if code != 0 {
            return Err(win32_error(code));
        }
        Ok(session)
    }
    fn handle(&self) -> HANDLE {
        self.client.as_ref().expect("active WLAN session").0
    }
    fn request(&self) -> Result<bool, String> {
        self.context.armed.store(true, Ordering::Release);
        let code = unsafe { WlanScan(self.handle(), &self.context.interface, None, None, None) };
        if code == ERROR_ACCESS_DENIED.0 {
            return Ok(false);
        }
        if code != 0 {
            return Err(win32_error(code));
        }
        match self.signals.recv_timeout(Duration::from_secs(4)) {
            Ok(ScanSignal::Complete) => Ok(true),
            Ok(ScanSignal::Failed) => Err("Windows could not complete the Wi-Fi scan. Refresh to try again.".into()),
            Err(_) => Err("Wi-Fi scan timed out. Refresh to try again.".into()),
        }
    }
}
impl Drop for ScanSession {
    fn drop(&mut self) {
        self.context.armed.store(false, Ordering::Release);
        if let Some(client) = self.client.take() {
            unsafe {
                WlanRegisterNotification(client.0, WLAN_NOTIFICATION_SOURCE_NONE, false, None, None, None, None);
            }
            // CloseHandle waits for callbacks even if unregister failed. The context
            // is still alive here and is only dropped after this function returns.
            drop(client);
        }
    }
}
fn win32_error(code: u32) -> String {
    windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(code)).message()
}

/// Requests fresh discovery on its dedicated worker. Cached results are published
/// first, and denied location access retains the existing saved-profile behavior.
pub fn scan_networks(publish_cached: impl FnMut(Networks)) -> Result<Networks, String> {
    let client = Client::open()?;
    let interface = client.interface().ok_or("This PC has no Wi-Fi adapter.")?;
    discover(
        &mut NativeDiscovery {
            client: Some(client),
            interface,
            session: None,
        },
        publish_cached,
    )
}

trait Discovery {
    fn read(&mut self) -> Result<Networks, String>;
    fn scan(&mut self) -> Result<bool, String>;
    fn saved(&mut self) -> Networks;
}
fn discover(backend: &mut impl Discovery, mut publish_cached: impl FnMut(Networks)) -> Result<Networks, String> {
    // Cached enumeration can fail while Windows is starting the adapter/service.
    // It is a preview, not a prerequisite for the one fresh scan the user asked for.
    if let Ok(cached) = backend.read() {
        publish_cached(cached.clone());
        if cached.location_off {
            return Ok(cached);
        }
    }
    if backend.scan()? {
        backend.read()
    } else {
        Ok(backend.saved())
    }
}
struct NativeDiscovery {
    client: Option<Client>,
    interface: GUID,
    session: Option<ScanSession>,
}
impl NativeDiscovery {
    fn client(&self) -> &Client {
        self.session
            .as_ref()
            .and_then(|session| session.client.as_ref())
            .or(self.client.as_ref())
            .expect("active WLAN session")
    }
}
impl Discovery for NativeDiscovery {
    fn read(&mut self) -> Result<Networks, String> {
        read_networks(self.client(), &self.interface)
    }
    fn scan(&mut self) -> Result<bool, String> {
        self.session = Some(ScanSession::new(
            self.client.take().expect("unscanned WLAN client"),
            self.interface,
        )?);
        self.session.as_ref().expect("scan session").request()
    }
    fn saved(&mut self) -> Networks {
        let connected = connection_name(self.client(), &self.interface);
        saved_networks(self.client(), &self.interface, connected.as_deref())
    }
}

/// Connects to a saved network. Windows reports failures later, so Ok means
/// Windows accepted the request.
pub fn connect(profile: &str) -> Result<(), String> {
    let client = Client::open()?;
    let interface = client.interface().ok_or("This PC has no Wi-Fi adapter.")?;
    let profile = HSTRING::from(profile);
    let parameters = WLAN_CONNECTION_PARAMETERS {
        wlanConnectionMode: wlan_connection_mode_profile,
        strProfile: PCWSTR(profile.as_ptr()),
        pDot11Ssid: std::ptr::null_mut(),
        pDesiredBssidList: std::ptr::null_mut(),
        dot11BssType: dot11_BSS_type_any,
        dwFlags: 0,
    };
    let code = unsafe { WlanConnect(client.0, &interface, &parameters, None) };
    if code == 0 {
        Ok(())
    } else {
        Err(windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(code)).message())
    }
}

/// The connected network's name. WLAN needs location access for it, so the
/// Network List Manager's name for the Wi-Fi connection is the fallback.
fn connected_name() -> Option<String> {
    let client = Client::open().ok()?;
    let interface = client.interface()?;
    connection_name(&client, &interface)
}

fn connection_name(client: &Client, interface: &GUID) -> Option<String> {
    client.current_ssid(interface).or_else(|| nlm_name(interface))
}

fn nlm_name(interface: &GUID) -> Option<String> {
    unsafe {
        let manager: INetworkListManager = CoCreateInstance(&NetworkListManager, None, CLSCTX_ALL).ok()?;
        let connections = manager.GetNetworkConnections().ok()?;
        loop {
            let mut item: [Option<INetworkConnection>; 1] = [None];
            let mut fetched = 0;
            connections.Next(&mut item, Some(&mut fetched)).ok()?;
            let connection = item[0].take().filter(|_| fetched == 1)?;
            if connection.GetAdapterId().ok().as_ref() == Some(interface) {
                return connection
                    .GetNetwork()
                    .and_then(|n| n.GetName())
                    .ok()
                    .map(|name| name.to_string());
            }
        }
    }
}

fn sorted(mut list: Vec<Network>) -> Vec<Network> {
    list.sort_by(|a, b| {
        b.connected
            .cmp(&a.connected)
            .then(b.profile.is_some().cmp(&a.profile.is_some()))
            .then(b.signal.cmp(&a.signal))
            .then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    list
}

fn utf16(units: &[u16]) -> String {
    let len = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..len])
}

fn ssid(ssid: &DOT11_SSID) -> String {
    let len = (ssid.uSSIDLength as usize).min(ssid.ucSSID.len());
    String::from_utf8_lossy(&ssid.ucSSID[..len]).into_owned()
}

/// A WLAN client handle, closed on drop.
struct Client(HANDLE);

impl Client {
    fn open() -> Result<Self, String> {
        let (mut version, mut handle) = (0, HANDLE::default());
        match unsafe { WlanOpenHandle(2, None, &mut version, &mut handle) } {
            0 => Ok(Self(handle)),
            _ => Err("Cannot reach the WLAN service.".into()),
        }
    }

    fn interface(&self) -> Option<GUID> {
        unsafe {
            let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
            if WlanEnumInterfaces(self.0, None, &mut list) != 0 || list.is_null() {
                return None;
            }
            let count = (*list).dwNumberOfItems as usize;
            let items = std::slice::from_raw_parts((*list).InterfaceInfo.as_ptr(), count);
            let guid = items.first().map(|info| info.InterfaceGuid);
            WlanFreeMemory(list.cast());
            guid
        }
    }

    fn current_ssid(&self, interface: &GUID) -> Option<String> {
        unsafe {
            let (mut size, mut data) = (0, std::ptr::null_mut());
            let code = WlanQueryInterface(
                self.0,
                interface,
                wlan_intf_opcode_current_connection,
                None,
                &mut size,
                &mut data,
                None,
            );
            if code != 0 || data.is_null() {
                return None;
            }
            let attributes = &*(data as *const WLAN_CONNECTION_ATTRIBUTES);
            let name = (attributes.isState == wlan_interface_state_connected)
                .then(|| ssid(&attributes.wlanAssociationAttributes.dot11Ssid));
            WlanFreeMemory(data);
            name
        }
    }

    /// Nearby networks, one entry per name. Err holds the Win32 error code.
    fn available(&self, interface: &GUID) -> Result<Vec<Network>, u32> {
        unsafe {
            let mut list: *mut WLAN_AVAILABLE_NETWORK_LIST = std::ptr::null_mut();
            let code = WlanGetAvailableNetworkList(self.0, interface, 0, None, &mut list);
            if code != 0 {
                return Err(code);
            }
            let count = (*list).dwNumberOfItems as usize;
            let items = std::slice::from_raw_parts((*list).Network.as_ptr(), count);
            let mut networks: Vec<Network> = Vec::new();
            for item in items {
                let name = ssid(&item.dot11Ssid);
                if name.is_empty() {
                    continue;
                }
                let profile = utf16(&item.strProfileName);
                let network = Network {
                    name,
                    profile: (!profile.is_empty()).then_some(profile),
                    connected: item.dwFlags & WLAN_AVAILABLE_NETWORK_CONNECTED != 0,
                    secured: item.bSecurityEnabled.as_bool(),
                    signal: Some(item.wlanSignalQuality),
                };
                // Windows lists a network once per profile and once without one.
                match networks.iter_mut().find(|n| n.name == network.name) {
                    Some(existing) => {
                        existing.connected |= network.connected;
                        existing.profile = existing.profile.take().or(network.profile);
                        existing.signal = existing.signal.max(network.signal);
                    }
                    None => networks.push(network),
                }
            }
            WlanFreeMemory(list.cast());
            Ok(sorted(networks))
        }
    }

    fn profiles(&self, interface: &GUID) -> Vec<String> {
        unsafe {
            let mut list: *mut WLAN_PROFILE_INFO_LIST = std::ptr::null_mut();
            if WlanGetProfileList(self.0, interface, None, &mut list) != 0 || list.is_null() {
                return Vec::new();
            }
            let count = (*list).dwNumberOfItems as usize;
            let items = std::slice::from_raw_parts((*list).ProfileInfo.as_ptr(), count);
            let names = items.iter().map(|info| utf16(&info.strProfileName)).collect();
            WlanFreeMemory(list.cast());
            names
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        unsafe {
            WlanCloseHandle(self.0, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn network(name: &str, saved: bool, connected: bool, signal: Option<u32>) -> Network {
        Network {
            name: name.into(),
            profile: saved.then(|| name.into()),
            connected,
            secured: true,
            signal,
        }
    }

    #[test]
    fn lists_connected_then_saved_then_strongest() {
        let list = sorted(vec![
            network("cafe", false, false, Some(90)),
            network("office", true, false, Some(40)),
            network("home", true, true, Some(70)),
            network("library", true, false, Some(80)),
        ]);
        let names: Vec<_> = list.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["home", "library", "office", "cafe"]);
    }

    struct FakeDiscovery {
        reads: std::collections::VecDeque<Result<Networks, String>>,
        result: Result<bool, String>,
        scans: usize,
    }
    impl Discovery for FakeDiscovery {
        fn read(&mut self) -> Result<Networks, String> {
            self.reads.pop_front().expect("expected network read")
        }
        fn scan(&mut self) -> Result<bool, String> {
            self.scans += 1;
            self.result.clone()
        }
        fn saved(&mut self) -> Networks {
            Networks {
                list: vec![network("saved", true, false, None)],
                location_off: true,
            }
        }
    }
    fn snapshot(name: Option<&str>, location_off: bool) -> Networks {
        Networks {
            list: name
                .map(|name| vec![network(name, false, false, Some(70))])
                .unwrap_or_default(),
            location_off,
        }
    }
    #[test]
    fn cached_empty_list_is_published_then_replaced_only_after_scan_completion() {
        let mut backend = FakeDiscovery {
            reads: [Ok(snapshot(None, false)), Ok(snapshot(Some("fresh"), false))].into(),
            result: Ok(true),
            scans: 0,
        };
        let mut shown = Vec::new();
        let final_list = discover(&mut backend, |cached| shown.push(cached)).unwrap();
        assert!(shown[0].list.is_empty());
        assert_eq!(backend.scans, 1);
        assert_eq!(final_list.list[0].name, "fresh");
        assert!(backend.reads.is_empty());
    }
    #[test]
    fn initial_cache_failure_does_not_abort_the_single_requested_fresh_scan() {
        let mut backend = FakeDiscovery {
            reads: [
                Err("adapter cache not ready".into()),
                Ok(snapshot(Some("fresh"), false)),
            ]
            .into(),
            result: Ok(true),
            scans: 0,
        };
        let mut shown = Vec::new();
        let final_list = discover(&mut backend, |cached| shown.push(cached)).unwrap();
        assert!(shown.is_empty(), "a cache error must not publish an empty snapshot");
        assert_eq!(final_list.list[0].name, "fresh");
        assert_eq!(backend.scans, 1, "a cache miss adds no retries");
        assert!(backend.reads.is_empty());
    }
    #[test]
    fn cache_failure_keeps_real_scan_and_final_read_errors_visible() {
        let mut scan_failure = FakeDiscovery {
            reads: [Err("cache not ready".into())].into(),
            result: Err("radio unavailable".into()),
            scans: 0,
        };
        assert_eq!(
            discover(&mut scan_failure, |_| panic!("no cached result")),
            Err("radio unavailable".into())
        );
        assert_eq!(scan_failure.scans, 1);
        let mut read_failure = FakeDiscovery {
            reads: [Err("cache not ready".into()), Err("fresh enumeration failed".into())].into(),
            result: Ok(true),
            scans: 0,
        };
        assert_eq!(
            discover(&mut read_failure, |_| panic!("no cached result")),
            Err("fresh enumeration failed".into())
        );
        assert_eq!(read_failure.scans, 1);
    }
    #[test]
    fn completed_empty_discovery_is_distinct_from_unknown_or_failed_discovery() {
        let mut backend = FakeDiscovery {
            reads: [Err("cache not ready".into()), Ok(snapshot(None, false))].into(),
            result: Ok(true),
            scans: 0,
        };
        let final_list = discover(&mut backend, |_| panic!("unknown cache is not an empty result")).unwrap();
        assert!(final_list.list.is_empty());
        assert!(!final_list.location_off);
        assert_eq!(backend.scans, 1);
    }
    #[test]
    fn failed_or_timed_out_scan_never_marks_cached_results_fresh() {
        for error in ["timed out", "scan failed"] {
            let mut backend = FakeDiscovery {
                reads: [Ok(snapshot(Some("cached"), false))].into(),
                result: Err(error.into()),
                scans: 0,
            };
            let mut shown = Vec::new();
            assert_eq!(discover(&mut backend, |cached| shown.push(cached)), Err(error.into()));
            assert_eq!(shown[0].list[0].name, "cached");
            assert_eq!(backend.scans, 1);
        }
    }
    #[test]
    fn location_denial_keeps_saved_profiles_without_scanning_or_claiming_nearby_results() {
        let cached = snapshot(Some("profile"), true);
        let mut backend = FakeDiscovery {
            reads: [Ok(cached.clone())].into(),
            result: Ok(true),
            scans: 0,
        };
        assert_eq!(discover(&mut backend, |_| {}).unwrap(), cached);
        assert_eq!(backend.scans, 0);
        let mut backend = FakeDiscovery {
            reads: [Ok(snapshot(None, false))].into(),
            result: Ok(false),
            scans: 0,
        };
        let saved = discover(&mut backend, |_| {}).unwrap();
        assert!(saved.location_off);
        assert_eq!(saved.list[0].name, "saved");
    }
    #[test]
    fn notifications_ignore_other_interfaces_sources_and_unarmed_sessions() {
        let interface = GUID::from_u128(1);
        let mut data = L2_NOTIFICATION_DATA {
            InterfaceGuid: interface,
            NotificationSource: WLAN_NOTIFICATION_SOURCE_ACM,
            NotificationCode: wlan_notification_acm_scan_complete.0 as u32,
            ..Default::default()
        };
        assert_eq!(scan_signal(&interface, &data), Some(ScanSignal::Complete));
        data.InterfaceGuid = GUID::from_u128(2);
        assert_eq!(scan_signal(&interface, &data), None);
        data.InterfaceGuid = interface;
        data.NotificationSource = WLAN_NOTIFICATION_SOURCE_NONE;
        assert_eq!(scan_signal(&interface, &data), None);
        data.NotificationSource = WLAN_NOTIFICATION_SOURCE_ACM;
        let (sender, receiver) = mpsc::sync_channel(1);
        let mut context = ScanContext {
            interface,
            signals: sender,
            armed: AtomicBool::new(false),
        };
        unsafe {
            scan_notification(&raw mut data, (&raw mut context).cast());
        }
        assert!(receiver.try_recv().is_err());
        context.armed.store(true, Ordering::Release);
        unsafe {
            scan_notification(&raw mut data, (&raw mut context).cast());
        }
        assert_eq!(receiver.try_recv().unwrap(), ScanSignal::Complete);
        context.armed.store(false, Ordering::Release);
        unsafe {
            scan_notification(&raw mut data, (&raw mut context).cast());
        }
        assert!(
            receiver.try_recv().is_err(),
            "late completion after timeout/teardown must be ignored"
        );
    }
}
