//! Optional hardware queries, run only on the command worker for Control Center.

use windows::{
    Devices::{
        Bluetooth::{
            BluetoothCacheMode, BluetoothConnectionStatus, BluetoothLEDevice,
            GenericAttributeProfile::GattCommunicationStatus,
        },
        Enumeration::DeviceInformation,
    },
    Storage::Streams::DataReader,
    Win32::{Devices::Display::*, Foundation::*, Graphics::Gdi::*},
    core::{BOOL, GUID, PCWSTR, w},
};

#[derive(Clone, Debug, PartialEq)]
pub struct Display {
    pub device: String,
    pub index: usize,
    pub name: String,
    pub percent: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Hardware {
    pub displays: Option<Vec<Display>>,
    pub batteries: Vec<(String, Option<u8>)>,
    pub networks: Option<super::wifi::Networks>,
}

#[derive(Clone)]
pub enum Request {
    Networks,
    Displays,
    Batteries,
    Brightness(Display, u32),
}

fn with_monitors(mut visit: impl FnMut(String, usize, &PHYSICAL_MONITOR)) {
    unsafe extern "system" fn collect(monitor: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        unsafe {
            (&mut *(data.0 as *mut Vec<HMONITOR>)).push(monitor);
        }
        BOOL(1)
    }
    let mut handles = Vec::<HMONITOR>::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(collect), LPARAM((&raw mut handles) as isize));
        for handle in handles {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            if !GetMonitorInfoW(handle, &raw mut info.monitorInfo).as_bool() {
                continue;
            }
            let device = decode(&info.szDevice);
            let mut count = 0;
            if GetNumberOfPhysicalMonitorsFromHMONITOR(handle, &mut count).is_err() || count == 0 || count > 32 {
                continue;
            }
            let mut monitors = vec![PHYSICAL_MONITOR::default(); count as usize];
            if GetPhysicalMonitorsFromHMONITOR(handle, &mut monitors).is_ok() {
                for (index, monitor) in monitors.iter().enumerate() {
                    visit(device.clone(), index, monitor);
                }
                let _ = DestroyPhysicalMonitors(&monitors);
            }
        }
    }
}

fn decode(text: &[u16]) -> String {
    String::from_utf16_lossy(&text[..text.iter().position(|&unit| unit == 0).unwrap_or(text.len())])
}

fn brightness(monitor: HANDLE) -> Option<(u32, u32, u32)> {
    let (mut capabilities, mut temperatures) = (0, 0);
    if unsafe { GetMonitorCapabilities(monitor, &mut capabilities, &mut temperatures) } == 0
        || capabilities & MC_CAPS_BRIGHTNESS == 0
    {
        return None;
    }
    let (mut min, mut current, mut max) = (0, 0, 0);
    let ok = unsafe { GetMonitorBrightness(monitor, &mut min, &mut current, &mut max) };
    (ok != 0 && min < max && (min..=max).contains(&current)).then_some((min, current, max))
}

fn displays() -> Vec<Display> {
    let mut displays = Vec::new();
    with_monitors(|device, index, monitor| {
        if let Some((min, current, max)) = brightness(monitor.hPhysicalMonitor) {
            let name = decode(&{ monitor.szPhysicalMonitorDescription });
            let percent = ((u64::from(current - min) * 100 + u64::from(max - min) / 2) / u64::from(max - min)) as u32;
            displays.push(Display {
                device,
                index,
                name,
                percent,
            });
        }
    });
    displays
}

fn set_brightness(display: Display, percent: u32) -> Result<(), String> {
    let mut changed = false;
    with_monitors(|device, index, monitor| {
        if device == display.device
            && index == display.index
            && decode(&{ monitor.szPhysicalMonitorDescription }) == display.name
            && let Some((min, _, max)) = brightness(monitor.hPhysicalMonitor)
        {
            let value = min + (u64::from(max - min) * u64::from(percent.min(100)) / 100) as u32;
            changed = unsafe { SetMonitorBrightness(monitor.hPhysicalMonitor, value) != 0 };
        }
    });
    if changed {
        Ok(())
    } else {
        Err("Windows could not change brightness for that display.".into())
    }
}

fn windows_batteries() -> Vec<(String, Option<u8>)> {
    use std::collections::HashMap;
    use windows::Win32::Devices::{DeviceAndDriverInstallation::*, Properties::*};

    const BATTERY: DEVPROPKEY = DEVPROPKEY {
        fmtid: GUID::from_u128(0x104ea319_6ee2_4701_bd47_8ddbF425bbe5),
        pid: 2,
    };
    let mut names = Vec::new();
    let mut readings = HashMap::new();
    let flags = CM_GETIDLIST_FILTER_ENUMERATOR | CM_GETIDLIST_FILTER_PRESENT;
    let property = |node, key, buffer: &mut [u8], expected| {
        let mut kind = DEVPROPTYPE::default();
        let mut len = buffer.len() as u32;
        let result = unsafe { CM_Get_DevNode_PropertyW(node, key, &mut kind, Some(buffer.as_mut_ptr()), &mut len, 0) };
        (result == CR_SUCCESS && kind == expected).then_some(len as usize)
    };
    for enumerator in [w!("BTHENUM"), w!("BTHLE"), w!("BTHHFENUM")] {
        let mut count = 0;
        if unsafe { CM_Get_Device_ID_List_SizeW(&mut count, enumerator, flags) } != CR_SUCCESS {
            continue;
        }
        let mut ids = vec![0; count as usize];
        if unsafe { CM_Get_Device_ID_ListW(enumerator, &mut ids, flags) } != CR_SUCCESS {
            continue;
        }
        for id in ids.split_inclusive(|&c| c == 0).take_while(|id| id.len() > 1) {
            let mut node = 0;
            if unsafe { CM_Locate_DevNodeW(&mut node, PCWSTR(id.as_ptr()), CM_LOCATE_DEVNODE_NORMAL) } != CR_SUCCESS {
                continue;
            }
            let mut container = [0; 16];
            if property(node, &DEVPKEY_Device_ContainerId, &mut container, DEVPROP_TYPE_GUID) != Some(16) {
                continue;
            }
            let mut battery = [0];
            if property(node, &BATTERY, &mut battery, DEVPROP_TYPE_BYTE) == Some(1) && battery[0] <= 100 {
                // A hands-free profile may publish the reading on a sibling node.
                readings
                    .entry(container)
                    .and_modify(|value: &mut Option<u8>| {
                        if *value != Some(battery[0]) {
                            *value = None;
                        }
                    })
                    .or_insert(Some(battery[0]));
            }
            let mut bytes = [0; 512];
            if let Some(len) = property(node, &DEVPKEY_Device_FriendlyName, &mut bytes, DEVPROP_TYPE_STRING) {
                let name = String::from_utf16_lossy(
                    &bytes[..len]
                        .chunks_exact(2)
                        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                        .take_while(|&unit| unit != 0)
                        .collect::<Vec<_>>(),
                );
                if !name.is_empty() {
                    names.push((name, container));
                }
            }
        }
    }
    names.sort_unstable();
    names.dedup();
    names
        .iter()
        .enumerate()
        .filter_map(|(index, (name, container))| {
            if index.checked_sub(1).is_some_and(|prev| names[prev].0 == *name)
                || names.get(index + 1).is_some_and(|next| next.0 == *name)
            {
                return None;
            }
            Some((name.clone(), Some(readings.get(container).copied().flatten()?)))
        })
        .collect()
}

fn batteries() -> Vec<(String, Option<u8>)> {
    let mut readings = windows_batteries();
    for (name, percent) in ble_batteries(&readings) {
        readings.push((name, percent));
    }
    readings
}

fn ble_batteries(known: &[(String, Option<u8>)]) -> Vec<(String, Option<u8>)> {
    const SERVICE: GUID = GUID::from_u128(0x0000180f_0000_1000_8000_00805f9b34fb);
    const LEVEL: GUID = GUID::from_u128(0x00002a19_0000_1000_8000_00805f9b34fb);
    let Ok(selector) = BluetoothLEDevice::GetDeviceSelectorFromConnectionStatus(BluetoothConnectionStatus::Connected)
    else {
        return Vec::new();
    };
    let Ok(devices) = DeviceInformation::FindAllAsyncAqsFilter(&selector).and_then(|op| op.join()) else {
        return Vec::new();
    };
    devices
        .into_iter()
        .take(16)
        .filter_map(|info| {
            let name = info.Name().ok()?.to_string();
            let name = name.strip_prefix("LE-").unwrap_or(&name);
            if known.iter().any(|(other, _)| other.eq_ignore_ascii_case(name)) {
                return None;
            }
            let device = BluetoothLEDevice::FromIdAsync(&info.Id().ok()?).ok()?.join().ok()?;
            if device.ConnectionStatus().ok()? != BluetoothConnectionStatus::Connected {
                return None;
            }
            let percent = (|| {
                let services = device
                    .GetGattServicesForUuidWithCacheModeAsync(SERVICE, BluetoothCacheMode::Cached)
                    .ok()?
                    .join()
                    .ok()?;
                let service = services.Services().ok()?.GetAt(0).ok()?;
                let characteristics = service
                    .GetCharacteristicsForUuidWithCacheModeAsync(LEVEL, BluetoothCacheMode::Cached)
                    .ok()?
                    .join()
                    .ok()?;
                let characteristic = characteristics.Characteristics().ok()?.GetAt(0).ok()?;
                let value = characteristic
                    .ReadValueWithCacheModeAsync(BluetoothCacheMode::Cached)
                    .ok()?
                    .join()
                    .ok()?;
                if value.Status().ok()? != GattCommunicationStatus::Success {
                    return None;
                }
                let percent = DataReader::FromBuffer(&value.Value().ok()?).ok()?.ReadByte().ok()?;
                (percent <= 100).then_some(percent)
            })();
            Some((name.to_owned(), percent))
        })
        .collect()
}

pub fn run(request: Request, state: &mut Hardware) -> Result<(), String> {
    match request {
        Request::Displays => state.displays = Some(displays()),
        Request::Networks => state.networks = Some(super::wifi::networks()?),
        Request::Batteries => state.batteries = batteries(),
        Request::Brightness(display, percent) => {
            set_brightness(display.clone(), percent)?;
            if let Some(shown) = state
                .displays
                .iter_mut()
                .flatten()
                .find(|d| d.device == display.device && d.index == display.index)
            {
                shown.percent = percent;
            }
        }
    }
    Ok(())
}
