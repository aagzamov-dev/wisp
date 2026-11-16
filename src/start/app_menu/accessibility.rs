//! Menu-only UI Automation tree. COM clients retain immutable action identities;
//! they never retain the UI thread's window state or a Launcher borrow.
use super::{ACCESS_FOCUS, ACCESS_INVOKE, Model, Row};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use windows::{
    Win32::{
        Foundation::*,
        System::{
            Com::SAFEARRAY,
            Ole::{SafeArrayCreateVector, SafeArrayDestroy, SafeArrayPutElement},
            Variant::{VARIANT, VT_I4},
        },
        UI::{Accessibility::*, WindowsAndMessaging::PostMessageW},
    },
    core::{Error, HRESULT, IUnknown, IUnknown_Vtbl, Interface, Result, implement, interface},
};

#[derive(Default)]
struct View {
    bounds: RECT,
    rows: Vec<RECT>,
    focus: Option<usize>,
}
pub(super) struct Snapshot {
    pub nonce: usize,
    hwnd: AtomicUsize,
    active: AtomicBool,
    rows: Vec<Row>,
    view: Mutex<View>,
}
impl Snapshot {
    pub fn new(nonce: usize, rows: &[Row]) -> Self {
        Self {
            nonce,
            hwnd: AtomicUsize::new(0),
            active: AtomicBool::new(true),
            rows: rows.to_vec(),
            view: Mutex::new(View::default()),
        }
    }
    pub fn attach(&self, hwnd: HWND) {
        self.hwnd.store(hwnd.0 as usize, Ordering::Release);
    }
    pub fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
    }
    pub fn focused_row(&self) -> Option<usize> {
        self.view.lock().ok().and_then(|v| v.focus)
    }
    pub fn update(&self, model: &Model, area: RECT, scale: f32) {
        if let Ok(mut view) = self.view.lock() {
            view.bounds = area;
            view.focus = model.focus;
            view.rows = model
                .rows
                .iter()
                .enumerate()
                .map(|(i, row)| {
                    let top = area.top + (model.top(i) * scale).round() as i32;
                    RECT {
                        left: area.left + (super::PAD * scale).round() as i32,
                        right: area.right - (super::PAD * scale).round() as i32,
                        top: top.max(area.top),
                        bottom: (top + (row.height() * scale).round() as i32)
                            .min(area.bottom)
                            .max(top.max(area.top)),
                    }
                })
                .collect();
        }
    }
    fn check(&self) -> Result<()> {
        if self.active.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(Error::from_hresult(windows::core::HRESULT(
                UIA_E_ELEMENTNOTAVAILABLE as i32,
            )))
        }
    }
    fn post(&self, row: Option<usize>, message: u32) -> Result<()> {
        self.check()?;
        let row = row
            .filter(|i| self.rows.get(*i).is_some_and(Row::enabled))
            .ok_or_else(|| Error::from_hresult(windows::core::HRESULT(UIA_E_ELEMENTNOTENABLED as i32)))?;
        unsafe {
            PostMessageW(
                Some(HWND(self.hwnd.load(Ordering::Acquire) as _)),
                message,
                WPARAM(self.nonce),
                LPARAM(row as isize),
            )
        }
    }
}

// windows-rs's generated provider traits return non-null COM interfaces. UIA
// requires S_OK/null for absent patterns, host providers, and tree neighbours.
// These menu-private ABI definitions keep the published IIDs/vtable layout,
// but expose raw out parameters so those optional results are representable.
#[interface("d6dd68d1-86fd-4332-8666-9abedea2d24c")]
unsafe trait MenuSimple: IUnknown {
    fn options(&self, out: *mut ProviderOptions) -> HRESULT;
    fn pattern(&self, id: UIA_PATTERN_ID, out: *mut *mut core::ffi::c_void) -> HRESULT;
    fn property(&self, id: UIA_PROPERTY_ID, out: *mut VARIANT) -> HRESULT;
    fn host(&self, out: *mut *mut core::ffi::c_void) -> HRESULT;
}
#[interface("f7063da8-8359-439c-9297-bbc5299a7d87")]
unsafe trait MenuFragment: IUnknown {
    fn navigate(&self, direction: NavigateDirection, out: *mut *mut core::ffi::c_void) -> HRESULT;
    fn runtime_id(&self, out: *mut *mut SAFEARRAY) -> HRESULT;
    fn bounds(&self, out: *mut UiaRect) -> HRESULT;
    fn embedded(&self, out: *mut *mut SAFEARRAY) -> HRESULT;
    fn focus(&self) -> HRESULT;
    fn root(&self, out: *mut *mut core::ffi::c_void) -> HRESULT;
}
#[interface("620ce2a5-ab8f-40a9-86cb-de3c75599b58")]
unsafe trait MenuRoot: IUnknown {
    fn at_point(&self, x: f64, y: f64, out: *mut *mut core::ffi::c_void) -> HRESULT;
    fn focused(&self, out: *mut *mut core::ffi::c_void) -> HRESULT;
}
unsafe fn optional_interface<T: Interface>(out: *mut *mut core::ffi::c_void, result: Result<T>) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    unsafe {
        out.write(std::ptr::null_mut());
    }
    match result {
        Ok(value) => {
            unsafe {
                out.write(value.into_raw());
            }
            S_OK
        }
        Err(error) if error.code() == E_NOTIMPL => S_OK,
        Err(error) => error.code(),
    }
}
unsafe fn value<T: Default>(out: *mut T, result: Result<T>) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    match result {
        Ok(value) => {
            unsafe {
                out.write(value);
            }
            S_OK
        }
        Err(error) => {
            unsafe {
                out.write(T::default());
            }
            error.code()
        }
    }
}
impl MenuSimple_Impl for Provider_Impl {
    unsafe fn options(&self, out: *mut ProviderOptions) -> HRESULT {
        unsafe { value(out, self.ProviderOptions()) }
    }
    unsafe fn pattern(&self, id: UIA_PATTERN_ID, out: *mut *mut core::ffi::c_void) -> HRESULT {
        unsafe { optional_interface(out, self.GetPatternProvider(id)) }
    }
    unsafe fn property(&self, id: UIA_PROPERTY_ID, out: *mut VARIANT) -> HRESULT {
        unsafe { value(out, self.GetPropertyValue(id)) }
    }
    unsafe fn host(&self, out: *mut *mut core::ffi::c_void) -> HRESULT {
        unsafe { optional_interface(out, self.HostRawElementProvider()) }
    }
}
impl MenuFragment_Impl for Provider_Impl {
    unsafe fn navigate(&self, direction: NavigateDirection, out: *mut *mut core::ffi::c_void) -> HRESULT {
        unsafe { optional_interface(out, self.Navigate(direction)) }
    }
    unsafe fn runtime_id(&self, out: *mut *mut SAFEARRAY) -> HRESULT {
        unsafe { value(out, self.GetRuntimeId()) }
    }
    unsafe fn bounds(&self, out: *mut UiaRect) -> HRESULT {
        unsafe { value(out, self.BoundingRectangle()) }
    }
    unsafe fn embedded(&self, out: *mut *mut SAFEARRAY) -> HRESULT {
        unsafe { value(out, self.GetEmbeddedFragmentRoots()) }
    }
    unsafe fn focus(&self) -> HRESULT {
        self.SetFocus().map_or_else(|e| e.code(), |()| S_OK)
    }
    unsafe fn root(&self, out: *mut *mut core::ffi::c_void) -> HRESULT {
        unsafe { optional_interface(out, self.FragmentRoot()) }
    }
}
impl MenuRoot_Impl for Provider_Impl {
    unsafe fn at_point(&self, x: f64, y: f64, out: *mut *mut core::ffi::c_void) -> HRESULT {
        unsafe { optional_interface(out, self.ElementProviderFromPoint(x, y)) }
    }
    unsafe fn focused(&self, out: *mut *mut core::ffi::c_void) -> HRESULT {
        unsafe { optional_interface(out, self.GetFocus()) }
    }
}

#[implement(MenuSimple, MenuFragment, MenuRoot, IInvokeProvider)]
struct Provider {
    snapshot: Arc<Snapshot>,
    row: Option<usize>,
}
impl Provider {
    fn simple(snapshot: Arc<Snapshot>, row: Option<usize>) -> IRawElementProviderSimple {
        let provider: MenuSimple = Self { snapshot, row }.into();
        // Same public IID and exact ABI as IRawElementProviderSimple, above.
        unsafe { IRawElementProviderSimple::from_raw(provider.into_raw()) }
    }
    fn fragment(&self, row: Option<usize>) -> Result<IRawElementProviderFragment> {
        Self::simple(self.snapshot.clone(), row).cast()
    }
}
#[allow(non_snake_case)]
impl Provider_Impl {
    fn ProviderOptions(&self) -> Result<ProviderOptions> {
        Ok(ProviderOptions_ServerSideProvider | ProviderOptions_UseComThreading)
    }
    fn GetPatternProvider(&self, id: UIA_PATTERN_ID) -> Result<IUnknown> {
        self.snapshot.check()?;
        if id == UIA_InvokePatternId && self.row.is_some() {
            Provider::simple(self.snapshot.clone(), self.row).cast()
        } else {
            Err(Error::from_hresult(E_NOTIMPL))
        }
    }
    fn GetPropertyValue(&self, id: UIA_PROPERTY_ID) -> Result<VARIANT> {
        self.snapshot.check()?;
        let row = self.row.and_then(|i| self.snapshot.rows.get(i));
        Ok(if id == UIA_ControlTypePropertyId {
            VARIANT::from(if row.is_some() {
                UIA_MenuItemControlTypeId.0
            } else {
                UIA_MenuControlTypeId.0
            })
        } else if id == UIA_NamePropertyId {
            VARIANT::from(row.map_or("App actions", |r| r.label))
        } else if id == UIA_HelpTextPropertyId {
            VARIANT::from(row.and_then(|r| r.reason).unwrap_or(""))
        } else if id == UIA_IsEnabledPropertyId {
            VARIANT::from(row.is_none_or(Row::enabled))
        } else if id == UIA_IsKeyboardFocusablePropertyId {
            VARIANT::from(row.is_some_and(Row::enabled))
        } else if id == UIA_HasKeyboardFocusPropertyId {
            VARIANT::from(self.row.is_some() && self.snapshot.view.lock().is_ok_and(|v| v.focus == self.row))
        } else if id == UIA_IsControlElementPropertyId || id == UIA_IsContentElementPropertyId {
            VARIANT::from(true)
        } else if id == UIA_IsOffscreenPropertyId {
            VARIANT::from(self.row.is_some_and(|i| {
                self.snapshot
                    .view
                    .lock()
                    .is_ok_and(|v| v.rows.get(i).is_none_or(|r| r.bottom <= r.top))
            }))
        } else {
            VARIANT::default()
        })
    }
    fn HostRawElementProvider(&self) -> Result<IRawElementProviderSimple> {
        self.snapshot.check()?;
        if self.row.is_none() {
            unsafe { UiaHostProviderFromHwnd(HWND(self.snapshot.hwnd.load(Ordering::Acquire) as _)) }
        } else {
            Err(Error::from_hresult(E_NOTIMPL))
        }
    }
}
#[allow(non_snake_case, non_upper_case_globals)]
impl Provider_Impl {
    fn Navigate(&self, direction: NavigateDirection) -> Result<IRawElementProviderFragment> {
        self.snapshot.check()?;
        let row = match (self.row, direction) {
            (Some(_), NavigateDirection_Parent) => None,
            (None, NavigateDirection_FirstChild) if !self.snapshot.rows.is_empty() => Some(0),
            (None, NavigateDirection_LastChild) if !self.snapshot.rows.is_empty() => Some(self.snapshot.rows.len() - 1),
            (Some(i), NavigateDirection_NextSibling) if i + 1 < self.snapshot.rows.len() => Some(i + 1),
            (Some(i), NavigateDirection_PreviousSibling) if i > 0 => Some(i - 1),
            _ => return Err(Error::from_hresult(E_NOTIMPL)),
        };
        self.fragment(row)
    }
    fn GetRuntimeId(&self) -> Result<*mut SAFEARRAY> {
        self.snapshot.check()?;
        let Some(row) = self.row else {
            return Ok(std::ptr::null_mut());
        };
        unsafe {
            let array = SafeArrayCreateVector(VT_I4, 0, 3);
            if array.is_null() {
                return Err(Error::from_hresult(E_OUTOFMEMORY));
            }
            for (index, value) in [UiaAppendRuntimeId as i32, self.snapshot.nonce as i32, row as i32]
                .iter()
                .enumerate()
            {
                if let Err(e) = SafeArrayPutElement(array, &(index as i32), std::ptr::from_ref(value).cast()) {
                    let _ = SafeArrayDestroy(array);
                    return Err(e);
                }
            }
            Ok(array)
        }
    }
    fn BoundingRectangle(&self) -> Result<UiaRect> {
        self.snapshot.check()?;
        let view = self.snapshot.view.lock().map_err(|_| Error::from_hresult(E_FAIL))?;
        let r = self.row.and_then(|i| view.rows.get(i)).unwrap_or(&view.bounds);
        Ok(UiaRect {
            left: r.left as f64,
            top: r.top as f64,
            width: (r.right - r.left).max(0) as f64,
            height: (r.bottom - r.top).max(0) as f64,
        })
    }
    fn GetEmbeddedFragmentRoots(&self) -> Result<*mut SAFEARRAY> {
        Ok(std::ptr::null_mut())
    }
    fn SetFocus(&self) -> Result<()> {
        self.snapshot.post(self.row, ACCESS_FOCUS)
    }
    fn FragmentRoot(&self) -> Result<IRawElementProviderFragmentRoot> {
        self.snapshot.check()?;
        Provider::simple(self.snapshot.clone(), None).cast()
    }
}
#[allow(non_snake_case)]
impl Provider_Impl {
    fn ElementProviderFromPoint(&self, x: f64, y: f64) -> Result<IRawElementProviderFragment> {
        self.snapshot.check()?;
        {
            let view = self.snapshot.view.lock().map_err(|_| Error::from_hresult(E_FAIL))?;
            let r = view.bounds;
            if x < r.left as f64 || x >= r.right as f64 || y < r.top as f64 || y >= r.bottom as f64 {
                return Err(Error::from_hresult(E_NOTIMPL));
            }
        }
        let row = self
            .snapshot
            .view
            .lock()
            .map_err(|_| Error::from_hresult(E_FAIL))?
            .rows
            .iter()
            .position(|r| x >= r.left as f64 && x < r.right as f64 && y >= r.top as f64 && y < r.bottom as f64);
        self.fragment(row)
    }
    fn GetFocus(&self) -> Result<IRawElementProviderFragment> {
        self.snapshot.check()?;
        let focus = self
            .snapshot
            .view
            .lock()
            .map_err(|_| Error::from_hresult(E_FAIL))?
            .focus;
        self.fragment(focus)
    }
}
impl IInvokeProvider_Impl for Provider_Impl {
    fn Invoke(&self) -> Result<()> {
        self.snapshot.post(self.row, ACCESS_INVOKE)
    }
}
pub(super) fn get_object(hwnd: HWND, wp: WPARAM, lp: LPARAM, snapshot: Arc<Snapshot>) -> LRESULT {
    let provider = Provider::simple(snapshot, None);
    unsafe { UiaReturnRawElementProvider(hwnd, wp, lp, &provider) }
}

pub(super) fn announce(snapshot: &Arc<Snapshot>, focus: Option<usize>, opened: bool) {
    if !snapshot.active.load(Ordering::Acquire) {
        return;
    }
    unsafe {
        if !UiaClientsAreListening().as_bool() {
            return;
        }
        if opened {
            let _ = UiaRaiseAutomationEvent(&Provider::simple(snapshot.clone(), None), UIA_MenuOpenedEventId);
        }
        if let Some(row) = focus {
            let _ = UiaRaiseAutomationEvent(
                &Provider::simple(snapshot.clone(), Some(row)),
                UIA_AutomationFocusChangedEventId,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retained_provider_rejects_after_window_is_gone() {
        let snapshot = Arc::new(Snapshot::new(
            77,
            &[Row {
                label: "Open",
                reason: None,
                glyph: "",
            }],
        ));
        let provider = Provider::simple(snapshot.clone(), Some(0));
        snapshot.deactivate();
        assert_eq!(
            unsafe { provider.GetPropertyValue(UIA_NamePropertyId) }
                .unwrap_err()
                .code()
                .0 as u32,
            UIA_E_ELEMENTNOTAVAILABLE
        );
        assert_eq!(
            unsafe { provider.cast::<IInvokeProvider>().unwrap().Invoke() }
                .unwrap_err()
                .code()
                .0 as u32,
            UIA_E_ELEMENTNOTAVAILABLE
        );
    }
    #[test]
    fn disabled_provider_exposes_reason_and_rejects_invoke() {
        let snapshot = Arc::new(Snapshot::new(
            78,
            &[Row {
                label: "Admin",
                reason: Some("Packaged apps cannot elevate"),
                glyph: "",
            }],
        ));
        let provider = Provider::simple(snapshot, Some(0));
        assert_eq!(
            unsafe { provider.cast::<IInvokeProvider>().unwrap().Invoke() }
                .unwrap_err()
                .code()
                .0 as u32,
            UIA_E_ELEMENTNOTENABLED
        );
        assert_eq!(
            unsafe { provider.GetPropertyValue(UIA_HelpTextPropertyId) }.unwrap(),
            VARIANT::from("Packaged apps cannot elevate")
        );
    }
    #[test]
    fn optional_com_results_initialize_null_and_return_success() {
        let provider = Provider::simple(Arc::new(Snapshot::new(79, &[])), None);
        let mut raw = std::ptr::dangling_mut::<core::ffi::c_void>();
        unsafe {
            let hr = (Interface::vtable(&provider).GetPatternProvider)(provider.as_raw(), UIA_ValuePatternId, &mut raw);
            assert_eq!(hr, S_OK);
            assert!(raw.is_null());
            let fragment = provider.cast::<IRawElementProviderFragment>().unwrap();
            raw = std::ptr::dangling_mut();
            let hr = (Interface::vtable(&fragment).Navigate)(fragment.as_raw(), NavigateDirection_FirstChild, &mut raw);
            assert_eq!(hr, S_OK);
            assert!(raw.is_null());
        }
    }
}
