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

/// One material policy for launcher, dock and its owned popups. Persistent
/// acrylic uses composition blur because DWM's system material falls back
/// to solid when the nonactivating dock is inactive.
pub fn apply_backdrop(
    hwnd: windows::Win32::Foundation::HWND,
    dark: bool,
    backdrop: crate::config::Backdrop,
    persistent: bool,
    color: &str,
) -> Palette {
    use crate::config::Backdrop;
    use windows::Win32::Graphics::Dwm::*;
    use windows::Win32::UI::Controls::MARGINS;
    let translucent = !matches!(backdrop, Backdrop::Solid | Backdrop::Color) && transparency_available();
    let glass = matches!(backdrop, Backdrop::Glass | Backdrop::LiquidGlass);
    let mut palette = Palette::new(dark, translucent);
    if backdrop == Backdrop::Color
        && let Some(rgb) = color
            .strip_prefix('#')
            .filter(|color| color.len() == 6)
            .and_then(|color| u32::from_str_radix(color, 16).ok())
    {
        let fill = rgba(rgb, 1.);
        let luminance = 0.2126 * fill.r + 0.7152 * fill.g + 0.0722 * fill.b;
        palette = Palette::new(luminance < 0.49, false);
        palette.background = fill;
        palette.background_solid = fill;
    }
    if translucent {
        match backdrop {
            Backdrop::Transparent => palette.background.a = 0.15,
            Backdrop::Glass => palette.background.a = 0.12,
            Backdrop::LiquidGlass => {
                palette.background.a = 0.09;
                palette.glass_rim = true;
            }
            _ => {}
        }
    }
    unsafe {
        let set = |attr, value: i32| DwmSetWindowAttribute(hwnd, attr, (&value as *const i32).cast(), 4);
        let _ = set(DWMWA_USE_IMMERSIVE_DARK_MODE, i32::from(dark));
        let _ = set(DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND.0);
        let _ = set(DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE as i32);
        let (accent, tint) = if translucent {
            match backdrop {
                Backdrop::Acrylic if persistent => (4, 0x70),
                Backdrop::Transparent => (2, 0x38),
                Backdrop::Glass => (4, 0x24),
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
        let flags = if glass && accent == 4 { 0 } else { 2 };
        let composition = composition_material(hwnd, accent, tint, flags, &palette);
        let shown = if accent == 0 {
            system && extended
        } else if !composition && glass {
            palette.background.a = 0.15;
            composition_material(hwnd, 2, 0x38, 2, &palette)
        } else {
            composition
        };
        if translucent && !shown {
            palette = Palette::new(dark, false);
        }
    }
    palette
}

/// The shared tint and hairline, in device-independent coordinates.
pub fn paint_background(
    rt: &windows::Win32::Graphics::Direct2D::ID2D1HwndRenderTarget,
    brush: &windows::Win32::Graphics::Direct2D::ID2D1SolidColorBrush,
    palette: &Palette,
    radius: f32,
) {
    let size = unsafe { rt.GetSize() };
    let (width, height) = (size.width, size.height);
    use windows::Win32::Graphics::Direct2D::Common::*;
    use windows::Win32::Graphics::Direct2D::D2D1_ROUNDED_RECT;
    unsafe {
        rt.Clear(Some(&palette.background));
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
