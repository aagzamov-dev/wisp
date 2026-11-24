use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;

/// Brand color from `assets/icon.svg`.
const BRAND: u32 = 0x0891B2;

fn rgba(hex: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((hex >> 16) & 0xFF) as f32 / 255.0,
        g: ((hex >> 8) & 0xFF) as f32 / 255.0,
        b: (hex & 0xFF) as f32 / 255.0,
        a,
    }
}

#[derive(Clone)]
pub struct Palette {
    pub glass_rim: bool,
    pub background: D2D1_COLOR_F,
    pub text: D2D1_COLOR_F,
    pub subtext: D2D1_COLOR_F,
    pub placeholder: D2D1_COLOR_F,
    pub row_selected: D2D1_COLOR_F,
    pub divider: D2D1_COLOR_F,
    pub accent: D2D1_COLOR_F,
    pub text_selection: D2D1_COLOR_F,
    pub tile: D2D1_COLOR_F,
    pub scrollbar: D2D1_COLOR_F,
    /// An opaque background color, for marks on the accent color.
    pub background_solid: D2D1_COLOR_F,
}

impl Palette {
    /// A translucent fill lets the acrylic or mica backdrop show through.
    pub fn new(dark: bool, translucent: bool) -> Self {
        if dark {
            Self {
                glass_rim: false,
                background: if translucent {
                    rgba(0x1C1C1C, 0.3)
                } else {
                    rgba(0x202020, 1.0)
                },
                text: rgba(0xF5F5F5, 1.0),
                subtext: rgba(0xFFFFFF, 0.58),
                placeholder: rgba(0xFFFFFF, 0.42),
                row_selected: rgba(0xFFFFFF, 0.08),
                divider: rgba(0xFFFFFF, 0.08),
                accent: rgba(0x22D3EE, 1.0),
                text_selection: rgba(BRAND, 0.6),
                tile: rgba(0xFFFFFF, 0.07),
                scrollbar: rgba(0xFFFFFF, 0.22),
                background_solid: rgba(0x1C1C1C, 1.0),
            }
        } else {
            Self {
                glass_rim: false,
                background: if translucent {
                    rgba(0xF3F3F3, 0.4)
                } else {
                    rgba(0xF9F9F9, 1.0)
                },
                text: rgba(0x1B1B1B, 1.0),
                subtext: rgba(0x000000, 0.58),
                placeholder: rgba(0x000000, 0.42),
                row_selected: rgba(0x000000, 0.055),
                divider: rgba(0x000000, 0.08),
                accent: rgba(BRAND, 1.0),
                text_selection: rgba(BRAND, 0.3),
                tile: rgba(0x000000, 0.05),
                scrollbar: rgba(0x000000, 0.25),
                background_solid: rgba(0xFFFFFF, 1.0),
            }
        }
    }
}

pub fn transparency_available() -> bool {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    use windows::Win32::System::Registry::*;
    use windows::core::w;
    let mut enabled = 1u32;
    let mut size = 4u32;
    let mut power = SYSTEM_POWER_STATUS::default();
    unsafe {
        let _ = RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("EnableTransparency"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut enabled as *mut u32).cast()),
            Some(&mut size),
        );
        let saver = GetSystemPowerStatus(&mut power).is_ok() && power.SystemStatusFlag != 0;
        enabled != 0 && !saver
    }
}

/// Glass is optional and must never override accessibility or system policy.
/// The native host-brush API additionally enforces Windows power policy.
pub fn island_blur_available() -> bool {
    use windows::Win32::UI::{Accessibility::*, WindowsAndMessaging::*};
    let mut contrast = HIGHCONTRASTW {
        cbSize: size_of::<HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
    let known = unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            contrast.cbSize,
            Some((&raw mut contrast).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .is_ok();
    known && contrast.dwFlags & HCF_HIGHCONTRASTON == HIGHCONTRASTW_FLAGS(0) && transparency_available()
}

/// Independent top-bar materials: tint the bar without blurring the desktop;
/// acrylic belongs only to the separately clipped island window.
pub fn apply_topbar(hwnd: windows::Win32::Foundation::HWND, opacity: u32, _island: bool) -> Palette {
    use windows::Win32::{
        Graphics::Gdi::{COLOR_WINDOW, COLOR_WINDOWTEXT, GetSysColor},
        UI::{Accessibility::*, WindowsAndMessaging::*},
    };
    let mut contrast = HIGHCONTRASTW {
        cbSize: size_of::<HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
    let high_contrast = unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            contrast.cbSize,
            Some((&raw mut contrast).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .is_ok()
        && contrast.dwFlags & HCF_HIGHCONTRASTON != HIGHCONTRASTW_FLAGS(0);
    let available = transparency_available() && !high_contrast;
    let mut palette = Palette::new(true, available);
    palette.background.a = if available { opacity.min(100) as f32 / 100. } else { 1. };
    if high_contrast {
        let system = |index| {
            let color = unsafe { GetSysColor(index) };
            rgba((color & 255) << 16 | (color & 0xFF00) | (color >> 16) & 255, 1.)
        };
        palette.background = system(COLOR_WINDOW);
        palette.background_solid = palette.background;
        palette.text = system(COLOR_WINDOWTEXT);
        palette.subtext = palette.text;
    }
    prepare_layered(hwnd);
    palette
}

/// ULW owns the complete visible shape. Native materials, frame, corners and
/// shadows must not draw a second rectangular surface behind its ARGB pixels.
pub(crate) fn prepare_layered(hwnd: windows::Win32::Foundation::HWND) {
    use windows::Win32::{Graphics::Dwm::*, Graphics::Gdi::SetWindowRgn, UI::Controls::MARGINS};
    let policy = layered_policy();
    unsafe {
        let set = |attr, value: i32| DwmSetWindowAttribute(hwnd, attr, (&value as *const i32).cast(), 4);
        let _ = set(DWMWA_SYSTEMBACKDROP_TYPE, policy.system_backdrop);
        let _ = set(DWMWA_NCRENDERING_POLICY, policy.nonclient);
        let _ = set(DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND.0);
        let _ = set(DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE as i32);
        let _ = DwmExtendFrameIntoClientArea(hwnd, &MARGINS::default());
        let _ = SetWindowRgn(hwnd, None, false);
    }
    composition_material(hwnd, policy.accent, 0, 0, &Palette::new(true, false));
}
struct LayeredPolicy {
    system_backdrop: i32,
    nonclient: i32,
    accent: u32,
}
fn layered_policy() -> LayeredPolicy {
    use windows::Win32::Graphics::Dwm::*;
    LayeredPolicy {
        system_backdrop: DWMSBT_NONE.0,
        nonclient: DWMNCRP_DISABLED.0,
        accent: 0,
    }
}

pub(crate) fn restore_native_nonclient(hwnd: windows::Win32::Foundation::HWND) {
    use windows::Win32::Graphics::Dwm::*;
    let policy = DWMNCRP_USEWINDOWSTYLE.0;
    unsafe {
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_NCRENDERING_POLICY, (&raw const policy).cast(), 4);
    }
}

/// Background-only overrides for Transparent and its Glass preset.
/// Missing values preserve each existing material's tint and choose its blur preset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MaterialOptions {
    pub transparency: Option<u32>,
    pub blur: Option<bool>,
}
impl MaterialOptions {
    pub fn transparency(self, backdrop: crate::config::Backdrop) -> u32 {
        self.transparency
            .unwrap_or(if backdrop == crate::config::Backdrop::Glass {
                36
            } else {
                85
            })
            .min(100)
    }
    pub fn blur(self, backdrop: crate::config::Backdrop) -> bool {
        use crate::config::Backdrop;
        match backdrop {
            Backdrop::Transparent | Backdrop::Glass => self.blur.unwrap_or(backdrop == Backdrop::Glass),
            Backdrop::Solid | Backdrop::Color => false,
            _ => true,
        }
    }
}

/// A pure recipe shared by native and per-pixel surfaces. `blur_ready` must
/// mean a real native material/host is available, never just transparent paint.
fn material_palette(
    dark: bool,
    backdrop: crate::config::Backdrop,
    color: &str,
    effects: bool,
    blur_ready: bool,
    options: MaterialOptions,
) -> Palette {
    use crate::config::Backdrop;
    let translucent =
        effects && (!options.blur(backdrop) || blur_ready) && !matches!(backdrop, Backdrop::Solid | Backdrop::Color);
    let mut palette = Palette::new(dark, translucent);
    if backdrop == Backdrop::Color {
        if let Some(rgb) = color
            .strip_prefix('#')
            .filter(|s| s.len() == 6)
            .and_then(|s| u32::from_str_radix(s, 16).ok())
        {
            let fill = rgba(rgb, 1.);
            palette = Palette::new(0.2126 * fill.r + 0.7152 * fill.g + 0.0722 * fill.b < 0.49, false);
            palette.background = fill;
            palette.background_solid = fill;
        }
    } else if translucent {
        palette.background.a = match backdrop {
            Backdrop::Glass | Backdrop::Transparent => (100 - options.transparency(backdrop)) as f32 / 100.,
            Backdrop::LiquidGlass => 0.09,
            Backdrop::Acrylic => 0.84,
            _ => {
                if dark {
                    0.3
                } else {
                    0.4
                }
            }
        };
        palette.glass_rim = backdrop == Backdrop::LiquidGlass;
    }
    palette
}

/// ULW alone has no desktop blur. Use a retained SurfaceMaterial to enable it.
pub fn apply_layered_surface(
    hwnd: windows::Win32::Foundation::HWND,
    dark: bool,
    backdrop: crate::config::Backdrop,
    color: &str,
    options: MaterialOptions,
) -> Palette {
    let accessible = apply_topbar(hwnd, 100, false);
    if high_contrast() {
        return accessible;
    }
    material_palette(dark, backdrop, color, island_blur_available(), false, options)
}

pub(crate) fn high_contrast() -> bool {
    use windows::Win32::UI::{Accessibility::*, WindowsAndMessaging::*};
    let mut value = HIGHCONTRASTW {
        cbSize: size_of::<HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
    let known = unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            value.cbSize,
            Some((&raw mut value).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .is_ok();
    !known || value.dwFlags & HCF_HIGHCONTRASTON != HIGHCONTRASTW_FLAGS(0)
}

/// Shared installation gate after any native operation that can dispatch a
/// nested callback. HWND identity and material generation must both survive.
pub(crate) fn material_is_current(expected: u64, current: u64, alive: bool, same_window: bool) -> bool {
    alive && same_window && expected == current
}

type MaterialPresentation = (i32, i32, i32, i32, u32, u32, u32);

/// One retained host per layered foreground. No desktop capture or per-frame
/// resource creation; caller owns installation/revalidation and foreground draw.
pub struct SurfaceMaterial {
    pub palette: Palette,
    fallback: Palette,
    foreground: windows::Win32::Foundation::HWND,
    host: Option<crate::dock::topbar_blur::BlurHost>,
    presented: std::cell::Cell<Option<MaterialPresentation>>,
}
impl SurfaceMaterial {
    pub fn prepare(
        owner: Option<windows::Win32::Foundation::HWND>,
        foreground: windows::Win32::Foundation::HWND,
        dark: bool,
        backdrop: crate::config::Backdrop,
        color: &str,
        options: MaterialOptions,
    ) -> Self {
        let fallback = apply_layered_surface(foreground, dark, backdrop, color, options);
        let effects = island_blur_available();
        let host = if effects && options.blur(backdrop) {
            match crate::dock::topbar_blur::BlurHost::new(owner.filter(|owner| *owner != foreground)) {
                Ok(host) => Some(host),
                Err(error) => {
                    crate::logging::event("material.prepare.failed", format_args!("error={error}"));
                    None
                }
            }
        } else {
            None
        };
        let palette = if host.is_some() {
            material_palette(dark, backdrop, color, effects, true, options)
        } else {
            fallback.clone()
        };
        Self {
            palette,
            fallback,
            foreground,
            host,
            presented: std::cell::Cell::new(None),
        }
    }
    /// False means the caller must repaint with the now-opaque palette.
    pub fn present(
        &mut self,
        bounds: windows::Win32::Foundation::RECT,
        dpi: u32,
        radius_dip: f32,
        opacity: f32,
    ) -> bool {
        let presentation = (
            bounds.left,
            bounds.top,
            bounds.right,
            bounds.bottom,
            dpi,
            radius_dip.to_bits(),
            opacity.to_bits(),
        );
        if self.presented.get() == Some(presentation)
            && self
                .host
                .as_ref()
                .is_none_or(|host| host.placement_current(self.foreground))
        {
            return true;
        }
        if let Some(host) = &mut self.host
            && let Err(error) = host.sync(self.foreground, bounds, dpi, radius_dip, opacity)
        {
            crate::logging::event("material.present.failed", format_args!("error={error}"));
            self.host.take();
            self.palette = self.fallback.clone();
            return false;
        }
        self.presented.set(Some(presentation));
        true
    }
    pub fn hide(&self) {
        self.presented.set(None);
        if let Some(host) = &self.host {
            host.hide();
        }
    }
}

/// Nonactivating surfaces need the composition acrylic policy as well as DWM's backdrop.
fn composition_material(
    hwnd: windows::Win32::Foundation::HWND,
    state: u32,
    alpha: u32,
    flags: u32,
    palette: &Palette,
) -> bool {
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows::core::{BOOL, s, w};
    #[repr(C)]
    struct Accent {
        state: u32,
        flags: u32,
        tint: u32,
        animation: u32,
    }
    #[repr(C)]
    struct Attribute {
        attribute: u32,
        data: *mut Accent,
        size: usize,
    }
    unsafe {
        let Ok(module) = GetModuleHandleW(w!("user32.dll")) else {
            return false;
        };
        let Some(proc) = GetProcAddress(module, s!("SetWindowCompositionAttribute")) else {
            return false;
        };
        let set: unsafe extern "system" fn(windows::Win32::Foundation::HWND, *mut Attribute) -> BOOL =
            std::mem::transmute(proc);
        let mut accent = Accent {
            state,
            flags,
            tint: (alpha << 24)
                | ((palette.background_solid.b * 255.0) as u32) << 16
                | ((palette.background_solid.g * 255.0) as u32) << 8
                | (palette.background_solid.r * 255.0) as u32,
            animation: 0,
        };
        let mut attribute = Attribute {
            attribute: 19,
            data: &mut accent,
            size: size_of::<Accent>(),
        };
        set(hwnd, &mut attribute).as_bool()
    }
}

/// Whole-window opacity for HWND-rendered content and its native backdrop.
/// Retain the layered style through the whole transition; clearing it midway
/// can invalidate the compositor's window surface and cause a visible flash.
#[derive(Default)]
pub struct WindowOpacity {
    alpha: Option<u8>,
}
impl WindowOpacity {
    pub fn present(&mut self, hwnd: windows::Win32::Foundation::HWND, progress: f32) -> windows::core::Result<()> {
        use windows::Win32::{Foundation::COLORREF, UI::WindowsAndMessaging::*};
        let alpha = (progress.clamp(0., 1.) * 255.).round() as u8;
        if self.alpha == Some(alpha) {
            return Ok(());
        }
        unsafe {
            if self.alpha.is_none() {
                let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style | WS_EX_LAYERED.0 as isize);
            }
            // Mark ownership before this fallible call so callers can always
            // restore the non-layered launcher if Windows rejects opacity.
            self.alpha = Some(alpha);
            SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA)?;
        }
        Ok(())
    }
    pub fn clear(&mut self, hwnd: windows::Win32::Foundation::HWND) {
        use windows::Win32::UI::WindowsAndMessaging::*;
        if self.alpha.take().is_none() {
            return;
        }
        unsafe {
            let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style & !(WS_EX_LAYERED.0 as isize));
        }
    }
}

// Reuse the existing transparent accent and optional acrylic blur primitive.
// Acrylic requires a nonzero native tint; keep only the minimum activation
// alpha here so the percentage remains owned by the foreground background.
fn clear_accent(backdrop: crate::config::Backdrop, options: MaterialOptions) -> (u32, u32) {
    if options.blur(backdrop) { (4, 1) } else { (2, 0) }
}

/// One material policy for launcher, dock and its owned popups. Persistent
/// acrylic uses composition blur because DWM's system material falls back
/// to solid when the nonactivating dock is inactive.
pub fn apply_backdrop(
    hwnd: windows::Win32::Foundation::HWND,
    dark: bool,
    backdrop: crate::config::Backdrop,
    persistent: bool,
    color: &str,
    options: MaterialOptions,
) -> Palette {
    use crate::config::Backdrop;
    use windows::Win32::Graphics::Dwm::*;
    use windows::Win32::UI::Controls::MARGINS;
    let translucent = !matches!(backdrop, Backdrop::Solid | Backdrop::Color) && island_blur_available();
    let glass = backdrop == Backdrop::LiquidGlass;
    let mut palette = material_palette(dark, backdrop, color, translucent, true, options);
    if high_contrast() {
        palette = apply_topbar(hwnd, 100, false);
    }
    unsafe {
        let set = |attr, value: i32| DwmSetWindowAttribute(hwnd, attr, (&value as *const i32).cast(), 4);
        let _ = set(DWMWA_USE_IMMERSIVE_DARK_MODE, i32::from(dark));
        let _ = set(DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND.0);
        let _ = set(DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE as i32);
        let (accent, tint) = if translucent {
            match backdrop {
                Backdrop::Acrylic if persistent => (4, 0x70),
                Backdrop::Transparent | Backdrop::Glass => clear_accent(backdrop, options),
                Backdrop::LiquidGlass => (4, 0x16),
                _ => (0, 0),
            }
        } else {
            (0, 0)
        };
        let material = if !translucent || accent != 0 {
            DWMSBT_NONE
        } else {
            match backdrop {
                Backdrop::Acrylic => DWMSBT_TRANSIENTWINDOW,
                Backdrop::Mica => DWMSBT_MAINWINDOW,
                _ => DWMSBT_NONE,
            }
        };
        {
            use windows::Win32::UI::WindowsAndMessaging::*;
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
            SetWindowLongPtrW(
                hwnd,
                GWL_STYLE,
                if !persistent {
                    style | WS_CAPTION.0 as isize
                } else {
                    style & !(WS_CAPTION.0 as isize)
                },
            );
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
        let system = set(DWMWA_SYSTEMBACKDROP_TYPE, material.0).is_ok();
        let margin = if !persistent { -1 } else { 0 };
        let extended = DwmExtendFrameIntoClientArea(
            hwnd,
            &MARGINS {
                cxLeftWidth: margin,
                cxRightWidth: margin,
                cyTopHeight: margin,
                cyBottomHeight: margin,
            },
        )
        .is_ok();
        if !persistent {
            let _ = windows::Win32::Graphics::Gdi::SetWindowRgn(hwnd, None, false);
        }
        // Match the transparent accent path, using acrylic blur with a
        // low nonzero tint and without the modern acrylic recipe.
        let flags = if (glass || matches!(backdrop, Backdrop::Transparent | Backdrop::Glass)) && accent == 4 {
            0
        } else {
            2
        };
        let composition = composition_material(hwnd, accent, tint, flags, &palette);
        let shown = if accent == 0 { system && extended } else { composition };
        if translucent && !shown {
            palette = material_palette(dark, backdrop, color, false, false, options);
        }
    }
    palette
}

/// The shared tint and hairline, in device-independent coordinates.
pub fn paint_background(
    rt: &windows::Win32::Graphics::Direct2D::ID2D1RenderTarget,
    brush: &windows::Win32::Graphics::Direct2D::ID2D1SolidColorBrush,
    palette: &Palette,
    radius: f32,
) {
    unsafe { rt.Clear(Some(&palette.background)) };
    paint_background_border(rt, brush, palette, radius);
}

pub(crate) fn paint_background_border(
    rt: &windows::Win32::Graphics::Direct2D::ID2D1RenderTarget,
    brush: &windows::Win32::Graphics::Direct2D::ID2D1SolidColorBrush,
    palette: &Palette,
    radius: f32,
) {
    let size = unsafe { rt.GetSize() };
    let (width, height) = (size.width, size.height);
    use windows::Win32::Graphics::Direct2D::Common::*;
    use windows::Win32::Graphics::Direct2D::D2D1_ROUNDED_RECT;
    unsafe {
        let (mut dpi, mut dpi_y) = (96.0, 96.0);
        rt.GetDpi(&mut dpi, &mut dpi_y);
        let pixel = 96.0 / dpi;
        brush.SetColor(&D2D1_COLOR_F {
            a: if palette.glass_rim {
                0.35
            } else if palette.text.r > 0.5 {
                0.14
            } else {
                0.12
            },
            ..palette.text
        });
        rt.DrawRoundedRectangle(
            &D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F {
                    left: pixel / 2.0,
                    top: pixel / 2.0,
                    right: width - pixel / 2.0,
                    bottom: height - pixel / 2.0,
                },
                radiusX: radius,
                radiusY: radius,
            },
            brush,
            pixel,
            None,
        );
    }
}

/// Windows takes ownership of the region on success. Composition acrylic is
/// clipped to the same animated outline as the Direct2D border.
pub fn shape(hwnd: windows::Win32::Foundation::HWND, width: i32, height: i32, radius: f32, dpi: u32) {
    use windows::Win32::Graphics::Gdi::*;
    unsafe {
        let diameter = (radius * 2.0 * dpi as f32 / 96.0).round() as i32;
        let region = CreateRoundRectRgn(0, 0, width + 1, height + 1, diameter, diameter);
        if !region.is_invalid() && SetWindowRgn(hwnd, Some(region), false) == 0 {
            let _ = DeleteObject(region.into());
        }
    }
}

#[cfg(test)]
mod layered_tests {
    use super::*;
    #[test]
    fn clear_recipe_distinguishes_intentional_transparency_from_failed_requested_blur() {
        use crate::config::Backdrop;
        for dark in [false, true] {
            for transparency in [0, 1, 36, 85, 99, 100, 101] {
                for blur in [false, true] {
                    let options = MaterialOptions {
                        transparency: Some(transparency),
                        blur: Some(blur),
                    };
                    let transparent = material_palette(dark, Backdrop::Transparent, "", true, true, options);
                    let glass = material_palette(dark, Backdrop::Glass, "", true, true, options);
                    assert_eq!(transparent.background.a, (100 - transparency.min(100)) as f32 / 100.);
                    assert_eq!(transparent.background.a, glass.background.a);
                    assert_eq!(transparent.text.a, 1.);
                    assert_eq!(transparent.text.a, glass.text.a);
                    assert_eq!(
                        clear_accent(Backdrop::Transparent, options),
                        clear_accent(Backdrop::Glass, options)
                    );
                    let unavailable = material_palette(dark, Backdrop::Transparent, "", true, false, options);
                    assert_eq!(
                        unavailable.background.a,
                        if blur { 1. } else { transparent.background.a }
                    );
                    let disabled = material_palette(dark, Backdrop::Transparent, "", false, true, options);
                    assert_eq!(disabled.background.a, 1., "OS/accessibility policy wins");
                }
            }
            let defaults = MaterialOptions::default();
            assert_eq!(
                material_palette(dark, Backdrop::Transparent, "", true, false, defaults)
                    .background
                    .a,
                0.15
            );
            assert_eq!(
                material_palette(dark, Backdrop::Glass, "", true, true, defaults)
                    .background
                    .a,
                0.64
            );
            assert_eq!(clear_accent(Backdrop::Glass, defaults), (4, 1));
            assert_eq!(clear_accent(Backdrop::Transparent, defaults), (2, 0));
            let override_options = MaterialOptions {
                transparency: Some(100),
                blur: Some(false),
            };
            assert_eq!(
                material_palette(dark, Backdrop::Acrylic, "", true, true, override_options)
                    .background
                    .a,
                0.84
            );
        }
    }

    #[test]
    fn requested_materials_keep_text_readable_over_black_and_white_desktops() {
        fn luminance(rgb: [f32; 3]) -> f32 {
            let linear = |value: f32| {
                if value <= 0.04045 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * linear(rgb[0]) + 0.7152 * linear(rgb[1]) + 0.0722 * linear(rgb[2])
        }
        for dark in [false, true] {
            for backdrop in [
                crate::config::Backdrop::Glass,
                crate::config::Backdrop::Acrylic,
                crate::config::Backdrop::Solid,
            ] {
                let palette = material_palette(dark, backdrop, "", true, true, MaterialOptions::default());
                let text = luminance([palette.text.r, palette.text.g, palette.text.b]);
                for desktop in [0., 1.] {
                    let fill = palette.background;
                    let blend = |channel| channel * fill.a + desktop * (1. - fill.a);
                    let background = luminance([blend(fill.r), blend(fill.g), blend(fill.b)]);
                    let contrast = (text.max(background) + 0.05) / (text.min(background) + 0.05);
                    assert!(
                        contrast >= 4.5,
                        "dark={dark}, backdrop={backdrop:?}, desktop={desktop}, contrast={contrast}"
                    );
                }
            }
        }
    }

    #[test]
    fn native_effects_cannot_restore_stale_or_removed_surface_materials() {
        assert!(material_is_current(3, 3, true, true));
        assert!(!material_is_current(3, 4, true, true));
        assert!(!material_is_current(3, 3, false, true));
        assert!(!material_is_current(3, 3, true, false));
    }

    #[test]
    fn shared_recipe_keeps_tint_and_resolves_opaque_when_effects_or_host_are_unavailable() {
        use crate::config::Backdrop;
        for dark in [true, false] {
            for backdrop in [
                Backdrop::Acrylic,
                Backdrop::Mica,
                Backdrop::Glass,
                Backdrop::LiquidGlass,
                Backdrop::Transparent,
            ] {
                let blurred = material_palette(dark, backdrop, "", true, true, MaterialOptions::default());
                assert!(blurred.background.a < 1.);
                for (effects, ready) in [(false, true), (true, false), (false, false)] {
                    if backdrop == Backdrop::Transparent && effects {
                        continue;
                    }
                    let fallback = material_palette(dark, backdrop, "", effects, ready, MaterialOptions::default());
                    assert_eq!(fallback.background.a, 1.);
                    assert!(!fallback.glass_rim);
                }
            }
            assert_eq!(
                material_palette(dark, Backdrop::Solid, "", true, true, MaterialOptions::default())
                    .background
                    .a,
                1.
            );
        }
        let custom = material_palette(true, Backdrop::Color, "#FFFFFF", true, true, MaterialOptions::default());
        assert_eq!(custom.background.r, 1.);
        assert_eq!(custom.background.a, 1.);
        assert!(custom.text.r < 0.5);
        let frosted = material_palette(true, Backdrop::Acrylic, "", true, true, MaterialOptions::default());
        let glass = material_palette(true, Backdrop::Glass, "", true, true, MaterialOptions::default());
        assert!(frosted.background.a > glass.background.a);
    }

    #[test]
    fn pixel_owned_surfaces_have_no_native_backdrop_frame_or_accent() {
        let policy = layered_policy();
        assert_eq!(policy.accent, 0);
        assert_eq!(policy.system_backdrop, windows::Win32::Graphics::Dwm::DWMSBT_NONE.0);
        assert_eq!(policy.nonclient, windows::Win32::Graphics::Dwm::DWMNCRP_DISABLED.0);
    }
}
