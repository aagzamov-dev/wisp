//! Optional Windows 11 host-backdrop glass beneath the existing ULW island.
//! No desktop pixels enter the process. One retained dispatcher/compositor is
//! shared by this UI thread; each island owns and closes only its own target.
//!
//! Public API: DWMWA_USE_HOSTBACKDROPBRUSH, DesktopWindowTarget,
//! CreateHostBackdropBrush and CompositionGeometricClip. Native system frames,
//! acrylic accent policies and desktop capture are deliberately not involved.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use windows::core::Interface;
use windows::{
    System::{DispatcherQueue, DispatcherQueueController},
    UI::Composition::{
        CompositionBorderMode, CompositionGeometricClip, CompositionRoundedRectangleGeometry, Compositor,
        Desktop::DesktopWindowTarget, SpriteVisual, Visual,
    },
    Win32::{
        Foundation::{
            E_INVALIDARG, E_UNEXPECTED, ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, RECT,
            WAIT_FAILED, WPARAM,
        },
        Graphics::{
            Dwm::{DWMWA_USE_HOSTBACKDROPBRUSH, DwmSetWindowAttribute},
            Gdi::{CreateRoundRectRgn, DeleteObject, SetWindowRgn},
        },
        System::{
            LibraryLoader::GetModuleHandleW,
            WinRT::{
                Composition::ICompositorDesktopInterop, CreateDispatcherQueueController, DQTAT_COM_NONE,
                DQTYPE_THREAD_CURRENT, DispatcherQueueOptions, RO_INIT_SINGLETHREADED, RoInitialize, RoUninitialize,
            },
        },
        UI::WindowsAndMessaging::*,
    },
    core::{Error, Result, w},
};
use windows_numerics::Vector2;

struct Apartment;
impl Apartment {
    fn new() -> Result<Self> {
        unsafe {
            RoInitialize(RO_INIT_SINGLETHREADED)?;
        }
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            RoUninitialize();
        }
    }
}

struct Runtime {
    compositor: RefCell<Option<Compositor>>,
    controller: Option<DispatcherQueueController>,
    _queue: Option<DispatcherQueue>,
    _apartment: Apartment,
}
thread_local! {
    // Kept for the UI-thread lifetime, even when the last island is disabled.
    // A compositor failure must not create a second dispatcher on the next try.
    static RUNTIME: RefCell<Option<Rc<Runtime>>> = const { RefCell::new(None) };
    static SHUTTING_DOWN: Cell<bool> = const { Cell::new(false) };
}
// Failed queue creation leaves no cached resource; successful queues outlive
// hosts and any subsequent compositor/target initialization failure.
fn retained<T>(slot: &RefCell<Option<Rc<T>>>, create: impl FnOnce() -> Result<T>) -> Result<Rc<T>> {
    if let Some(value) = slot.borrow().as_ref() {
        return Ok(value.clone());
    }
    let value = Rc::new(create()?);
    *slot.borrow_mut() = Some(value.clone());
    Ok(value)
}

impl Runtime {
    fn acquire() -> Result<Rc<Self>> {
        if SHUTTING_DOWN.get() {
            return Err(Error::from_hresult(E_UNEXPECTED));
        }
        RUNTIME.with(|slot| {
            retained(slot, || {
                let apartment = Apartment::new()?;
                let (controller, queue) = if let Ok(queue) = DispatcherQueue::GetForCurrentThread() {
                    (None, Some(queue))
                } else {
                    let controller = unsafe {
                        CreateDispatcherQueueController(DispatcherQueueOptions {
                            dwSize: size_of::<DispatcherQueueOptions>() as u32,
                            threadType: DQTYPE_THREAD_CURRENT,
                            // OleInitialize already establishes Wisp's UI STA.
                            apartmentType: DQTAT_COM_NONE,
                        })?
                    };
                    // No fallible operation after queue creation: retain ownership
                    // even if later compositor/target initialization fails.
                    (Some(controller), None)
                };
                Ok(Self {
                    compositor: RefCell::new(None),
                    controller,
                    _queue: queue,
                    _apartment: apartment,
                })
            })
        })
    }
    fn compositor(&self) -> Result<Compositor> {
        if let Some(compositor) = self.compositor.borrow().as_ref() {
            return Ok(compositor.clone());
        }
        let compositor = Compositor::new()?;
        *self.compositor.borrow_mut() = Some(compositor.clone());
        Ok(compositor)
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.compositor.get_mut().take();
        if let Some(controller) = &self.controller {
            // Never wait for a current-thread queue from that same UI thread.
            let _ = controller.ShutdownQueueAsync();
        }
    }
}

/// Called after Launcher (and therefore every island target) has dropped, while
/// the UI STA and OLE still exist. Current-thread queues require a message pump
/// during shutdown; waiting with IAsyncAction::join here would deadlock.
pub(crate) fn shutdown_current_thread() -> Result<()> {
    SHUTTING_DOWN.set(true);
    let Some(shared) = RUNTIME.with(|slot| slot.borrow_mut().take()) else {
        return Ok(());
    };
    let mut runtime = match Rc::try_unwrap(shared) {
        Ok(runtime) => runtime,
        Err(shared) => {
            RUNTIME.with(|slot| *slot.borrow_mut() = Some(shared));
            return Err(Error::from_hresult(E_UNEXPECTED));
        }
    };
    runtime.compositor.get_mut().take();
    let Some(controller) = runtime.controller.take() else {
        return Ok(());
    };
    let mut terminal = false;
    let result = (|| {
        let action = controller.ShutdownQueueAsync()?;
        let (drained, quit) = drain_queue(
            // Started is the public WinRT AsyncStatus ABI value 0. The inferred
            // action type avoids another direct crate dependency.
            || Ok(action.Status()?.0 == 0),
            |quit| {
                let mut message = MSG::default();
                while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                    if message.message == WM_QUIT {
                        *quit = Some(message.wParam.0 as i32);
                    } else {
                        unsafe {
                            let _ = TranslateMessage(&message);
                            DispatchMessageW(&message);
                        }
                    }
                }
            },
            || {
                // Exit-only wait also observes completion that posts no Win32
                // message. No timer or same-thread async join is introduced.
                let waited = unsafe { MsgWaitForMultipleObjectsEx(None, 50, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
                if waited == WAIT_FAILED {
                    Err(Error::from_thread())
                } else {
                    Ok(())
                }
            },
        );
        if let Some(code) = quit {
            unsafe {
                PostQuitMessage(code);
            }
        }
        drained?;
        terminal = true;
        action.GetResults()
    })();
    if result.is_err() && !terminal {
        // Preserve resources for the remaining thread lifetime on an unknown
        // native failure. This is not confirmation that a pending queue drained;
        // the caller reports the error and TLS teardown is only a fallback.
        runtime.controller = Some(controller);
        RUNTIME.with(|slot| *slot.borrow_mut() = Some(Rc::new(runtime)));
    }
    result
}

// Kept separate from native queue creation so ordering, quit preservation and
// failure termination can be checked without initializing Composition.
fn drain_queue(
    mut pending: impl FnMut() -> Result<bool>,
    mut pump: impl FnMut(&mut Option<i32>),
    mut wait: impl FnMut() -> Result<()>,
) -> (Result<()>, Option<i32>) {
    let mut quit = None;
    let result = (|| {
        while pending()? {
            pump(&mut quit);
            if !pending()? {
                break;
            }
            wait()?;
        }
        Ok(())
    })();
    (result, quit)
}

struct Window(HWND);
unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}
impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.0);
        }
    }
}

trait TargetLifecycle {
    fn detach(&self);
    fn close(&self);
}
impl TargetLifecycle for DesktopWindowTarget {
    fn detach(&self) {
        let _ = self.SetRoot(None::<&Visual>);
    }
    fn close(&self) {
        let _ = self.Close();
    }
}
// Own the native target immediately after creation, so every later fallible
// brush/geometry/root step receives the same cleanup as a complete host.
struct ClosingTarget<T: TargetLifecycle>(T);
impl<T: TargetLifecycle> Drop for ClosingTarget<T> {
    fn drop(&mut self) {
        self.0.detach();
        self.0.close();
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Shape {
    width: i32,
    height: i32,
    radius: f32,
}
impl Shape {
    fn new(rect: RECT, dpi: u32, radius_dip: f32) -> Result<Self> {
        let width = rect
            .right
            .checked_sub(rect.left)
            .ok_or_else(|| Error::from_hresult(E_INVALIDARG))?;
        let height = rect
            .bottom
            .checked_sub(rect.top)
            .ok_or_else(|| Error::from_hresult(E_INVALIDARG))?;
        if width <= 2 || height <= 2 || dpi == 0 || !radius_dip.is_finite() || radius_dip < 0. {
            return Err(Error::from_hresult(E_INVALIDARG));
        }
        let radius = (radius_dip * dpi as f32 / 96.).min(width.min(height) as f32 / 2.);
        Ok(Self { width, height, radius })
    }
    fn inset_diameter(self) -> i32 {
        // Round upward: the GDI footprint stays conservatively inside the
        // antialiased foreground, including fractional-DPI corners.
        (2. * (self.radius - 1.).max(0.)).ceil() as i32
    }
    fn footprint(self) -> (i32, i32, i32) {
        (self.width, self.height, self.inset_diameter())
    }
}

/// A fade changes only Composition opacity. Native placement and clipping are
/// independent, and explicit hiding invalidates only the visible placement.
#[derive(Default)]
struct Presentation {
    shape: Option<Shape>,
    placement: Cell<Option<(isize, RECT)>>,
    opacity: Option<f32>,
}
impl Presentation {
    fn needs_clip(&self, shape: Shape) -> bool {
        self.shape.is_none_or(|old| old.footprint() != shape.footprint())
    }
    fn needs_placement(&self, island: HWND, rect: RECT, native_current: bool) -> bool {
        !native_current || self.placement.get() != Some((island.0 as isize, rect))
    }
    fn hide(&self, native_visible: bool) -> bool {
        self.placement.take().is_some() || native_visible
    }
}

pub(crate) struct BlurHost {
    // These fields release before window; the shared runtime outlives targets.
    _target: ClosingTarget<DesktopWindowTarget>,
    visual: SpriteVisual,
    geometry: CompositionRoundedRectangleGeometry,
    _clip: CompositionGeometricClip,
    _runtime: Rc<Runtime>,
    presentation: Presentation,
    window: Window,
}
impl BlurHost {
    /// Activation can reorder owned popup siblings without changing any cached
    /// geometry. Check the native invariant before trusting a presentation memo.
    pub(crate) fn placement_current(&self, foreground: HWND) -> bool {
        self.presentation.placement.get().is_some_and(|(expected, _)| {
            expected == foreground.0 as isize
                && unsafe {
                    IsWindowVisible(self.window.0).as_bool()
                        && GetWindow(self.window.0, GW_HWNDPREV).is_ok_and(|above| above == foreground)
                }
        })
    }
    pub(crate) fn new(owner: Option<HWND>) -> Result<Self> {
        if !crate::theme::island_blur_available() {
            return Err(Error::from_hresult(E_INVALIDARG));
        }
        let window = unsafe {
            let instance = GetModuleHandleW(None)?.into();
            let class = WNDCLASSW {
                hInstance: instance,
                lpfnWndProc: Some(window_proc),
                lpszClassName: w!("Wisp.TopBar.Glass"),
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
                return Err(Error::from_thread());
            }
            // The foreground owns all input. Region clipping additionally
            // prevents an invisible rectangular footprint at its corners.
            Window(CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP,
                class.lpszClassName,
                w!("Wisp island glass"),
                WS_POPUP | WS_DISABLED,
                0,
                0,
                1,
                1,
                owner,
                None,
                Some(instance),
                None,
            )?)
        };
        unsafe {
            let enabled = windows::core::BOOL(1);
            DwmSetWindowAttribute(
                window.0,
                DWMWA_USE_HOSTBACKDROPBRUSH,
                (&raw const enabled).cast(),
                size_of_val(&enabled) as u32,
            )?;
        }
        let runtime = Runtime::acquire()?;
        let compositor = runtime.compositor()?;
        let target = ClosingTarget(unsafe {
            compositor
                .cast::<ICompositorDesktopInterop>()?
                .CreateDesktopWindowTarget(window.0, false)?
        });
        let visual = compositor.CreateSpriteVisual()?;
        visual.SetBrush(&compositor.CreateHostBackdropBrush()?)?;
        visual.SetBorderMode(CompositionBorderMode::Soft)?;
        let geometry = compositor.CreateRoundedRectangleGeometry()?;
        let clip = compositor.CreateGeometricClipWithGeometry(&geometry)?;
        visual.SetClip(&clip)?;
        target.0.SetRoot(&visual)?;
        Ok(Self {
            _target: target,
            visual,
            geometry,
            _clip: clip,
            _runtime: runtime,
            presentation: Presentation::default(),
            window,
        })
    }

    pub(crate) fn sync(&mut self, island: HWND, rect: RECT, dpi: u32, radius_dip: f32, reveal: f32) -> Result<()> {
        let result = self.present(island, rect, dpi, radius_dip, reveal);
        if result.is_err() {
            self.hide();
        }
        result
    }
    fn present(&mut self, island: HWND, rect: RECT, dpi: u32, radius_dip: f32, reveal: f32) -> Result<()> {
        if !reveal.is_finite() {
            return Err(Error::from_hresult(E_INVALIDARG));
        }
        let shape = Shape::new(rect, dpi, radius_dip)?;
        let clip_changed = self.presentation.needs_clip(shape);
        if clip_changed {
            self.hide();
        }
        if self.presentation.shape != Some(shape) {
            let size = Vector2 {
                X: shape.width as f32,
                Y: shape.height as f32,
            };
            self.visual.SetSize(size)?;
            self.geometry.SetSize(size)?;
            self.geometry.SetCornerRadius(Vector2 {
                X: shape.radius,
                Y: shape.radius,
            })?;
        }
        if clip_changed {
            unsafe {
                let region = CreateRoundRectRgn(
                    1,
                    1,
                    shape.width - 1,
                    shape.height - 1,
                    shape.inset_diameter(),
                    shape.inset_diameter(),
                );
                if region.is_invalid() {
                    return Err(Error::from_thread());
                }
                if SetWindowRgn(self.window.0, Some(region), false) == 0 {
                    let _ = DeleteObject(region.into());
                    return Err(Error::from_thread());
                }
            }
        }
        self.presentation.shape = Some(shape);
        let opacity = reveal.clamp(0., 1.);
        if self.presentation.opacity != Some(opacity) {
            self.visual.SetOpacity(opacity)?;
            self.presentation.opacity = Some(opacity);
        }
        if self
            .presentation
            .needs_placement(island, rect, self.placement_current(island))
        {
            unsafe {
                SetWindowPos(
                    self.window.0,
                    Some(island),
                    rect.left,
                    rect.top,
                    shape.width,
                    shape.height,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                )?;
            }
            self.presentation.placement.set(Some((island.0 as isize, rect)));
            // A rejected native ordering must follow the existing host-failure
            // path, rather than attempting SetWindowPos on every future frame.
            if !self.placement_current(island) {
                return Err(Error::from_hresult(E_UNEXPECTED));
            }
        }
        Ok(())
    }
    pub(crate) fn hide(&self) {
        if self
            .presentation
            .hide(unsafe { IsWindowVisible(self.window.0).as_bool() })
        {
            unsafe {
                let _ = ShowWindow(self.window.0, SW_HIDE);
            }
        }
    }
}
impl Drop for BlurHost {
    fn drop(&mut self) {
        self.hide();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use windows::Win32::Graphics::Gdi::PtInRegion;

    #[test]
    fn fades_reuse_clip_and_placement_until_hidden_moved_or_reowned() {
        let rect = RECT {
            left: -400,
            top: 24,
            right: 0,
            bottom: 248,
        };
        let island = HWND(1 as _);
        let shape = Shape::new(rect, 120, 18.).unwrap();
        let mut state = Presentation::default();
        assert!(state.needs_clip(shape));
        assert!(state.needs_placement(island, rect, false));
        state.shape = Some(shape);
        state.placement.set(Some((island.0 as isize, rect)));
        for opacity in [0.08, 0.25, 0.5, 0.9, 1.] {
            state.opacity = Some(opacity);
            assert!(!state.needs_clip(shape));
            assert!(!state.needs_placement(island, rect, true));
        }
        assert!(state.needs_placement(HWND(2 as _), rect, true));
        assert!(state.needs_placement(
            island,
            RECT {
                left: -399,
                right: 1,
                ..rect
            },
            true
        ));
        assert!(state.hide(false));
        assert!(!state.hide(false));
        assert!(state.needs_placement(island, rect, false));
        assert!(!state.needs_clip(shape));
    }

    #[test]
    fn fractional_radius_reuses_integer_clip_but_size_and_dpi_rebuild_it() {
        let rect = RECT {
            right: 400,
            bottom: 224,
            ..Default::default()
        };
        let first = Shape::new(rect, 96, 18.1).unwrap();
        let state = Presentation {
            shape: Some(first),
            ..Default::default()
        };
        let fractional = Shape::new(rect, 96, 18.2).unwrap();
        assert_ne!(first, fractional);
        assert!(!state.needs_clip(fractional));
        assert!(state.needs_clip(Shape::new(rect, 96, 18.6).unwrap()));
        assert!(state.needs_clip(Shape::new(rect, 120, 18.1).unwrap()));
        assert!(state.needs_clip(Shape::new(RECT { bottom: 225, ..rect }, 96, 18.1).unwrap()));
    }

    #[test]
    fn externally_reordered_or_hidden_companion_repairs_unchanged_geometry_once() {
        let foreground = HWND(1 as _);
        let rect = RECT {
            right: 200,
            bottom: 28,
            ..Default::default()
        };
        let state = Presentation::default();
        state.placement.set(Some((foreground.0 as isize, rect)));
        assert!(!state.needs_placement(foreground, rect, true));
        // Both external hiding and a different HWND above the companion break
        // the native invariant despite unchanged cached identity/rectangle.
        assert!(state.needs_placement(foreground, rect, false));
        assert!(!state.needs_placement(foreground, rect, true));
        assert!(state.hide(false));
        assert!(state.needs_placement(foreground, rect, false));
        assert!(
            state.hide(true),
            "native visibility wins over a cached-hidden companion"
        );
        assert!(!state.hide(false));
    }

    #[test]
    fn shutdown_pumps_until_terminal_and_preserves_quit_without_reposting_it() {
        let mut checks = [true, true, false].into_iter();
        let pumps = Cell::new(0);
        let waits = Cell::new(0);
        let (result, quit) = drain_queue(
            || Ok(checks.next().unwrap()),
            |quit| {
                pumps.set(pumps.get() + 1);
                *quit = Some(7);
            },
            || {
                waits.set(waits.get() + 1);
                Ok(())
            },
        );
        assert!(result.is_ok());
        assert_eq!((pumps.get(), waits.get(), quit), (1, 1, Some(7)));
    }

    #[test]
    fn shutdown_status_or_wait_errors_stop_without_spinning() {
        let calls = Cell::new(0);
        let (result, _) = drain_queue(
            || {
                calls.set(calls.get() + 1);
                Err(Error::from_hresult(E_INVALIDARG))
            },
            |_| panic!("must not pump after a status error"),
            || panic!("must not wait after a status error"),
        );
        assert!(result.is_err());
        assert_eq!(calls.get(), 1);
        let waits = Cell::new(0);
        let (result, quit) = drain_queue(
            || Ok(true),
            |quit| *quit = Some(3),
            || {
                waits.set(waits.get() + 1);
                Err(Error::from_hresult(E_INVALIDARG))
            },
        );
        assert!(result.is_err());
        assert_eq!((waits.get(), quit), (1, Some(3)));
    }

    struct FakeTarget(Rc<RefCell<Vec<&'static str>>>);
    impl TargetLifecycle for FakeTarget {
        fn detach(&self) {
            self.0.borrow_mut().push("detach");
        }
        fn close(&self) {
            self.0.borrow_mut().push("close");
        }
    }
    struct FakeWindow(Rc<RefCell<Vec<&'static str>>>);
    impl Drop for FakeWindow {
        fn drop(&mut self) {
            self.0.borrow_mut().push("destroy window");
        }
    }

    #[test]
    fn target_guard_closes_before_window_on_partial_initialization_failure() {
        fn initialize(log: &Rc<RefCell<Vec<&'static str>>>) -> Result<()> {
            let _window = FakeWindow(log.clone());
            let _target = ClosingTarget(FakeTarget(log.clone()));
            // Models failure in any of the fallible native brush/clip/root steps.
            Err(Error::from_hresult(E_INVALIDARG))
        }
        let log = Rc::new(RefCell::new(Vec::new()));
        assert!(initialize(&log).is_err());
        assert_eq!(*log.borrow(), ["detach", "close", "destroy window"]);
    }

    #[test]
    fn complete_hosts_close_each_target_once_before_their_windows() {
        struct Host {
            _target: ClosingTarget<FakeTarget>,
            _window: FakeWindow,
        }
        let log = Rc::new(RefCell::new(Vec::new()));
        for _ in 0..3 {
            let host = Host {
                _target: ClosingTarget(FakeTarget(log.clone())),
                _window: FakeWindow(log.clone()),
            };
            drop(host);
        }
        assert_eq!(*log.borrow(), ["detach", "close", "destroy window"].repeat(3));
    }

    #[test]
    fn retained_queue_survives_host_failure_disable_and_multiple_monitors() {
        struct FakeQueue {
            drops: Rc<Cell<usize>>,
        }
        impl Drop for FakeQueue {
            fn drop(&mut self) {
                self.drops.set(self.drops.get() + 1);
            }
        }
        let slot = RefCell::new(None);
        let creations = Cell::new(0);
        let drops = Rc::new(Cell::new(0));
        let create = || {
            creations.set(creations.get() + 1);
            Ok(FakeQueue { drops: drops.clone() })
        };
        // The production acquisition boundary retains a created queue before
        // attempting fallible compositor/target work.
        let first = retained(&slot, create).unwrap();
        let target_failure: Result<()> = Err(Error::from_hresult(E_INVALIDARG));
        assert!(target_failure.is_err());
        drop(first);
        let reenabled = retained(&slot, create).unwrap();
        let second_monitor = retained(&slot, create).unwrap();
        assert!(Rc::ptr_eq(&reenabled, &second_monitor));
        assert_eq!(creations.get(), 1);
        drop(reenabled);
        drop(second_monitor);
        assert_eq!(drops.get(), 0);
        slot.borrow_mut().take();
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn failed_queue_creation_can_be_attempted_at_a_later_explicit_transition() {
        let slot: RefCell<Option<Rc<()>>> = RefCell::new(None);
        assert!(retained(&slot, || Err(Error::from_hresult(E_INVALIDARG))).is_err());
        assert!(slot.borrow().is_none());
        assert!(retained(&slot, || Ok(())).is_ok());
    }

    #[test]
    fn rejects_invalid_geometry_and_scales_radius_for_fractional_dpi() {
        let bounds = RECT {
            left: -1920,
            top: -100,
            right: -1600,
            bottom: -65,
        };
        let shape = Shape::new(bounds, 120, 7.).unwrap();
        assert_eq!((shape.width, shape.height, shape.radius), (320, 35, 8.75));
        assert!(Shape::new(bounds, 0, 7.).is_err());
        assert!(Shape::new(bounds, 120, f32::NAN).is_err());
        assert!(
            Shape::new(
                RECT {
                    left: i32::MIN,
                    right: i32::MAX,
                    ..bounds
                },
                96,
                7.
            )
            .is_err()
        );
        assert!(
            Shape::new(
                RECT {
                    right: bounds.left,
                    ..bounds
                },
                96,
                7.
            )
            .is_err()
        );
    }

    #[test]
    fn actual_gdi_footprint_stays_inside_foreground_pixels_at_every_supported_scale() {
        for dpi in [96, 120, 144, 168, 192, 240, 288] {
            for height_dip in [16., 20., 28., 44.] {
                let height = (height_dip * dpi as f32 / 96_f32).round() as i32;
                for width in [height, 160, 420] {
                    let shape = Shape::new(
                        RECT {
                            right: width,
                            bottom: height,
                            ..Default::default()
                        },
                        dpi,
                        7_f32.min(height_dip * 0.3),
                    )
                    .unwrap();
                    let region = unsafe {
                        CreateRoundRectRgn(
                            1,
                            1,
                            width - 1,
                            height - 1,
                            shape.inset_diameter(),
                            shape.inset_diameter(),
                        )
                    };
                    assert!(!region.is_invalid());
                    let mut outside = None;
                    let mut inside_count = 0;
                    for y in 0..height {
                        for x in 0..width {
                            if !unsafe { PtInRegion(region, x, y) }.as_bool() {
                                continue;
                            }
                            inside_count += 1;
                            let edge_x = (x.min(width - 1 - x)) as f32 + 0.5;
                            let edge_y = (y.min(height - 1 - y)) as f32 + 0.5;
                            let coverage = if edge_x < shape.radius && edge_y < shape.radius {
                                (shape.radius
                                    - ((shape.radius - edge_x).powi(2) + (shape.radius - edge_y).powi(2)).sqrt()
                                    + 0.5)
                                    .clamp(0., 1.)
                            } else {
                                1.
                            };
                            if coverage < 0.5 {
                                outside = Some((x, y));
                            }
                        }
                    }
                    unsafe {
                        let _ = DeleteObject(region.into());
                    }
                    assert!(inside_count > 0);
                    assert_eq!(outside, None, "dpi={dpi}, shape={shape:?}");
                }
            }
        }
    }
}
