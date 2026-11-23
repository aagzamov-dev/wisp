//! Premultiplied BGRA presentation for the top bar, without DWM frame tint.
use super::*;
use windows::Win32::{
    Foundation::{POINT, SIZE},
    Graphics::Gdi::*,
    UI::WindowsAndMessaging::*,
};

struct MemoryDc(HDC);
#[derive(Clone, Copy)]
pub(super) enum Contour {
    Rectangle,
    Round(f32),
    Island,
}
/// Applied after all drawing, so native presentation and tested pixels share one contour.
fn finish_pixels(pixels: &mut [u8], size: D2D_SIZE_U, dpi: u32, contour: Contour, opacity: u8) {
    let radius = match contour {
        Contour::Rectangle => 0.,
        Contour::Round(dip) => dip * dpi as f32 / 96.,
        Contour::Island => island_radius(size.height as f32 * 96. / dpi as f32) * dpi as f32 / 96.,
    }
    .clamp(0., size.height.min(size.width) as f32 / 2.);
    if radius == 0. && opacity == 255 {
        return;
    }
    if opacity == 0 {
        pixels.fill(0);
        return;
    }
    let width = size.width as usize;
    let height = size.height as usize;
    for y in 0..height {
        if matches!(contour, Contour::Rectangle) && opacity == 255 && y.min(height - 1 - y) as f32 + 0.5 >= radius {
            continue;
        }
        for x in 0..width {
            let edge_x = x.min(width - 1 - x) as f32 + 0.5;
            let edge_y = y.min(height - 1 - y) as f32 + 0.5;
            let coverage = if edge_x < radius && edge_y < radius {
                (radius - ((radius - edge_x).powi(2) + (radius - edge_y).powi(2)).sqrt() + 0.5).clamp(0., 1.)
            } else {
                1.
            };
            let alpha = (coverage * opacity as f32).round() as u32;
            if alpha != 255 {
                for c in &mut pixels[(y * width + x) * 4..(y * width + x) * 4 + 4] {
                    *c = ((*c as u32 * alpha + 127) / 255) as u8;
                }
            }
            // The antialiased mask may round a1-alpha hit pixel to0. Preserve
            // input at pixel centers inside the rounded surface, never in its exterior.
            if !matches!(contour, Contour::Rectangle) && coverage >= 0.5 {
                let pixel = &mut pixels[(y * width + x) * 4..(y * width + x) * 4 + 4];
                if pixel[3] == 0 {
                    pixel[3] = 1;
                }
            }
        }
    }
}
impl Drop for MemoryDc {
    fn drop(&mut self) {
        let _ = unsafe { DeleteDC(self.0) };
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BufferLayout {
    width: u32,
    height: u32,
    bytes: usize,
}
impl BufferLayout {
    fn new(size: D2D_SIZE_U) -> Result<Self> {
        let invalid = || windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG);
        if size.width == 0 || size.height == 0 || size.width > i32::MAX as u32 || size.height > i32::MAX as u32 {
            return Err(invalid());
        }
        let bytes = (size.width as usize)
            .checked_mul(size.height as usize)
            .and_then(|n| n.checked_mul(4))
            .filter(|n| *n <= isize::MAX as usize)
            .ok_or_else(invalid)?;
        Ok(Self {
            width: size.width,
            height: size.height,
            bytes,
        })
    }
}
#[derive(Default)]
struct BufferState {
    allocation: Option<BufferLayout>,
    bound: bool,
}
impl BufferState {
    fn needs_allocation(&self, layout: BufferLayout) -> bool {
        self.allocation != Some(layout)
    }
    fn selected(&mut self, layout: BufferLayout) {
        self.allocation = Some(layout);
        self.bound = false;
    }
    fn slice_len(&self, size: D2D_SIZE_U) -> Result<usize> {
        let layout = BufferLayout::new(size)?;
        if !self.bound || self.allocation != Some(layout) {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_FAIL,
                "Layered buffer is not bound at the requested size",
            ));
        }
        Ok(layout.bytes)
    }
}
/// Allocation/selection failures leave the old buffer untouched; a rejected
/// candidate drops here, before any allocation metadata can be committed.
fn allocate_selected<T, R, E>(
    allocate: impl FnOnce() -> std::result::Result<T, E>,
    select: impl FnOnce(&T) -> std::result::Result<R, E>,
) -> std::result::Result<(T, R), E> {
    let next = allocate()?;
    let previous = select(&next)?;
    Ok((next, previous))
}
fn bind_selected<E>(
    state: &mut BufferState,
    bind: impl FnOnce() -> std::result::Result<(), E>,
) -> std::result::Result<(), E> {
    state.bound = false;
    bind()?;
    state.bound = true;
    Ok(())
}
struct Dib {
    bitmap: HBITMAP,
    bits: *mut u8,
    layout: BufferLayout,
}
impl Dib {
    fn new(dc: HDC, layout: BufferLayout) -> Result<Self> {
        unsafe {
            let mut bits = std::ptr::null_mut();
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: layout.width as i32,
                    biHeight: -(layout.height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let bitmap = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
            let dib = Self {
                bitmap,
                bits: bits.cast(),
                layout,
            };
            if dib.bits.is_null() {
                return Err(windows::core::Error::new(
                    windows::Win32::Foundation::E_FAIL,
                    "Layered bitmap has no pixel buffer",
                ));
            }
            std::ptr::write_bytes(dib.bits, 0, layout.bytes);
            Ok(dib)
        }
    }
}
impl Drop for Dib {
    fn drop(&mut self) {
        let _ = unsafe { DeleteObject(self.bitmap.into()) };
    }
}

pub(super) struct Surface {
    dc: MemoryDc,
    dib: Option<Dib>,
    buffer: BufferState,
    previous: HGDIOBJ,
    bits: *mut u8,
    dc_target: ID2D1DCRenderTarget,
    target: Target,
    prepared: Option<(u32, u32, u32)>,
}
impl Surface {
    pub fn new(factory: &ID2D1Factory, palette: &Palette) -> Result<Self> {
        unsafe {
            let dc = MemoryDc(CreateCompatibleDC(None));
            if dc.0.is_invalid() {
                return Err(windows::core::Error::from_thread());
            }
            let dc_target = factory.CreateDCRenderTarget(&properties())?;
            let rt: ID2D1RenderTarget = dc_target.cast()?;
            let target = Target {
                brush: rt.CreateSolidColorBrush(&palette.text, None)?,
                rt,
                window: None,
                bitmaps: HashMap::new(),
                tray: HashMap::new(),
                media_shapes: HashMap::new(),
                launcher_shape: None,
                size: D2D_SIZE_U::default(),
            };
            Ok(Self {
                dc,
                dib: None,
                buffer: BufferState::default(),
                previous: HGDIOBJ::default(),
                bits: std::ptr::null_mut(),
                dc_target,
                target,
                prepared: None,
            })
        }
    }
    fn render(&mut self, size: D2D_SIZE_U, dpi: u32, f: impl FnOnce(&mut Target) -> Result<()>) -> Result<()> {
        self.render_with_bind(size, dpi, |target, dc, rect| unsafe { target.BindDC(dc, rect) }, f)
    }
    fn render_with_bind(
        &mut self,
        size: D2D_SIZE_U,
        dpi: u32,
        bind: impl FnOnce(&ID2D1DCRenderTarget, HDC, &RECT) -> Result<()>,
        f: impl FnOnce(&mut Target) -> Result<()>,
    ) -> Result<()> {
        self.prepared = None;
        unsafe {
            let layout = BufferLayout::new(size)?;
            if self.buffer.needs_allocation(layout) {
                let (next, previous) = allocate_selected(
                    || Dib::new(self.dc.0, layout),
                    |next| {
                        let previous = SelectObject(self.dc.0, next.bitmap.into());
                        if previous.is_invalid() || previous.0 as isize == -1 {
                            Err(windows::core::Error::new(
                                windows::Win32::Foundation::E_FAIL,
                                "Could not select layered bitmap",
                            ))
                        } else {
                            Ok(previous)
                        }
                    },
                )?;
                if self.dib.is_none() {
                    self.previous = previous;
                }
                self.bits = next.bits;
                self.buffer.selected(next.layout);
                self.target.size = D2D_SIZE_U::default();
                self.dib = Some(next);
            }
            if !self.buffer.bound {
                self.target.size = D2D_SIZE_U::default();
                bind_selected(&mut self.buffer, || {
                    bind(
                        &self.dc_target,
                        self.dc.0,
                        &RECT {
                            left: 0,
                            top: 0,
                            right: size.width as i32,
                            bottom: size.height as i32,
                        },
                    )
                })?;
                self.target.size = size;
            }
            self.target.rt.SetDpi(dpi as f32, dpi as f32);
            self.target.rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            self.target.rt.BeginDraw();
            let drawn = f(&mut self.target);
            let ended = self.target.rt.EndDraw(None, None);
            drawn.and(ended)
        }
    }
    pub fn paint_with_alpha(
        &mut self,
        hwnd: HWND,
        dpi: u32,
        formats: &Formats,
        presentation: (Contour, u8),
        alpha: u8,
        f: impl FnOnce(&mut Target, &Formats) -> Result<()>,
    ) -> Result<()> {
        let area = window_area(hwnd)?;
        let size = area_size(area);
        self.prepare(size, dpi, presentation, |target| f(target, formats))?;
        self.present_at(hwnd, area, alpha)
    }
    fn prepare(
        &mut self,
        size: D2D_SIZE_U,
        dpi: u32,
        presentation: (Contour, u8),
        draw: impl FnOnce(&mut Target) -> Result<()>,
    ) -> Result<()> {
        self.render(size, dpi, draw)?;
        self.finish(size, dpi, presentation)?;
        self.prepared = Some((size.width, size.height, dpi));
        Ok(())
    }
    /// Reuses the completed, masked DIB without drawing or reapplying the mask.
    pub fn present_cached(&self, hwnd: HWND, dpi: u32, alpha: u8) -> Result<()> {
        let area = window_area(hwnd)?;
        self.present_prepared(area_size(area), dpi, || self.present_at(hwnd, area, alpha))
    }
    pub fn cached_matches(&self, hwnd: HWND, dpi: u32) -> Result<bool> {
        let size = area_size(window_area(hwnd)?);
        Ok(self.prepared == Some((size.width, size.height, dpi)))
    }
    fn present_prepared(&self, size: D2D_SIZE_U, dpi: u32, present: impl FnOnce() -> Result<()>) -> Result<()> {
        self.validate_prepared(size, dpi)?;
        present()
    }
    fn validate_prepared(&self, size: D2D_SIZE_U, dpi: u32) -> Result<()> {
        if self.prepared != Some((size.width, size.height, dpi)) {
            return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL));
        }
        self.pixels(size).map(|_| ())
    }
    fn present_at(&self, hwnd: HWND, area: RECT, alpha: u8) -> Result<()> {
        let size = area_size(area);
        unsafe {
            UpdateLayeredWindow(
                hwnd,
                None,
                Some(&POINT {
                    x: area.left,
                    y: area.top,
                }),
                Some(&SIZE {
                    cx: size.width as i32,
                    cy: size.height as i32,
                }),
                Some(self.dc.0),
                Some(&POINT::default()),
                windows::Win32::Foundation::COLORREF(0),
                Some(&BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8,
                    BlendFlags: 0,
                    SourceConstantAlpha: alpha,
                    AlphaFormat: AC_SRC_ALPHA as u8,
                }),
                ULW_ALPHA,
            )
        }
    }
    fn pixels(&self, size: D2D_SIZE_U) -> Result<&[u8]> {
        let length = self.buffer.slice_len(size)?;
        if self.bits.is_null() {
            return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL));
        }
        Ok(unsafe { std::slice::from_raw_parts(self.bits, length) })
    }
    fn finish(&mut self, size: D2D_SIZE_U, dpi: u32, presentation: (Contour, u8)) -> Result<()> {
        let length = self.pixels(size)?.len();
        let pixels = unsafe { std::slice::from_raw_parts_mut(self.bits, length) };
        finish_pixels(pixels, size, dpi, presentation.0, presentation.1);
        Ok(())
    }
}
fn window_area(hwnd: HWND) -> Result<RECT> {
    let mut area = RECT::default();
    unsafe {
        GetWindowRect(hwnd, &mut area)?;
    }
    Ok(area)
}
fn area_size(area: RECT) -> D2D_SIZE_U {
    D2D_SIZE_U {
        width: (area.right - area.left).max(1) as u32,
        height: (area.bottom - area.top).max(1) as u32,
    }
}
impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            if !self.previous.is_invalid() {
                SelectObject(self.dc.0, self.previous);
            }
        }
    }
}
fn properties() -> D2D1_RENDER_TARGET_PROPERTIES {
    D2D1_RENDER_TARGET_PROPERTIES {
        r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        ..Default::default()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn topbar_tray_zero_frame_retains_icon_pixels_through_cached_fade() -> Result<()> {
        let renderer = Renderer::new()?;
        let shown = [Shown {
            icon: crate::tray_host::Icon {
                hwnd: 1,
                uid: 1,
                guid: None,
                callback: 1,
                version: 4,
                state: 0,
                tip: "Tray fixture".into(),
                image: None,
                revision: 1,
                pid: 0,
                exes: Vec::new(),
                promoted: None,
                order: None,
                accepted: true,
            },
            pixels: Some(Arc::new(IconPixels {
                width: 16,
                height: 16,
                bgra: [32, 96, 224, 255].repeat(16 * 16),
            })),
        }];
        for dpi in [96, 120, 144, 192] {
            for clear in [false, true] {
                let mut palette = Palette::new(true, true);
                if clear {
                    palette.background.a = 0.;
                }
                let scale = dpi as f32 / 96.;
                let size = D2D_SIZE_U {
                    width: (56. * scale).round() as u32,
                    height: (56. * scale).round() as u32,
                };
                for initial in [0., 0.25] {
                    let visual = crate::dock::topbar::model::surface_visual(initial);
                    let frame = TopbarFlyoutFrame {
                        dpi,
                        palette: &palette,
                        shown: &shown,
                        overflow: &[0],
                        hover: None,
                        presentation: (visual.scale, visual.opacity),
                    };
                    let mut surface = Surface::new(&renderer.factory, &palette)?;
                    surface.prepare(size, dpi, frame.cached_contour(), |target| {
                        draw_topbar_overflow_content(target, &frame)
                    })?;
                    let before = surface.pixels(size)?.to_vec();
                    let icon_pixels = before
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .filter(|p| p[2] > 200 && p[0] < 50 && p[3] == 255)
                        .count();
                    assert!(icon_pixels > 100, "cached tray lost icon: {dpi}/{clear}/{initial}");
                    assert_eq!(&before[..4], &[0, 0, 0, 0]);
                    for alpha in [0u8, 24, 128, 255, 96, 0, 255] {
                        surface.present_prepared(size, dpi, || {
                            // Substitute only the HWND submission; read the actual prepared DIB.
                            let presented = surface
                                .pixels(size)?
                                .iter()
                                .map(|v| (*v as u32 * alpha as u32 + 127) / 255)
                                .collect::<Vec<_>>();
                            assert_eq!(&presented[..4], &[0, 0, 0, 0]);
                            if alpha == 255 {
                                assert!(
                                    presented
                                        .as_chunks::<4>()
                                        .0
                                        .iter()
                                        .filter(|p| p[2] > 200 && p[0] < 50 && p[3] == 255)
                                        .count()
                                        > 100
                                );
                            }
                            Ok(())
                        })?;
                        assert_eq!(surface.pixels(size)?, before);
                    }
                }
            }
        }
        Ok(())
    }
    #[test]
    fn clear_rounded_menus_keep_interior_input_alpha_without_filling_exterior_corners() {
        for dpi in [96, 120, 144, 192] {
            for opacity in [1, 64, 255] {
                let size = D2D_SIZE_U { width: 120, height: 80 };
                let mut pixels = vec![0; size.width as usize * size.height as usize * 4];
                finish_pixels(&mut pixels, size, dpi, Contour::Round(12.), opacity);
                let alpha = |x: usize, y: usize| pixels[(y * size.width as usize + x) * 4 + 3];
                assert_eq!(alpha(0, 0), 0);
                assert_eq!(alpha(119, 79), 0);
                assert_eq!(alpha(60, 0), 1);
                assert_eq!(alpha(60, 40), 1);
                assert_eq!(alpha(0, 40), 1);
                finish_pixels(&mut pixels, size, dpi, Contour::Round(12.), 0);
                assert!(pixels.iter().all(|pixel| *pixel == 0));
            }
        }
    }

    #[test]
    fn island_clock_pixels_survive_repeated_resize_dpi_reveal_and_transparent_tint() -> Result<()> {
        let renderer = Renderer::new()?;
        let status = crate::dock::status::Status {
            time: "11:32".into(),
            date: "Sun 15 Nov".into(),
            ..Default::default()
        };
        let (date_width, time_width, _) = renderer.measure_topbar(&status.date, &status.time, "")?;
        let mut palette = Palette::new(true, true);
        let mut surface = Surface::new(&renderer.factory, &palette)?;
        for tint in [0., 0.7] {
            palette.background.a = tint;
            for (dpi, width, reveal) in [
                (96, 208., 0.08),
                (144, 240., 0.5),
                (120, 230., 1.),
                (192, 208., 1.),
                (96, 208., 1.),
            ] {
                let scale = dpi as f32 / 96.;
                let size = D2D_SIZE_U {
                    width: (width * scale).round() as u32,
                    height: (20. * scale).round() as u32,
                };
                surface.prepare(size, dpi, (Contour::Island, 255), |target| {
                    draw_island_content(
                        target,
                        &renderer.formats,
                        IslandFrame {
                            dpi,
                            palette: &palette,
                            status: &status,
                            track: None,
                            previous_track: None,
                            track_fade: 1.,
                            reveal,
                            mix: 0.,
                            width,
                            compact: false,
                            date_width,
                            time_width,
                            hover: None,
                            hover_level: 0.,
                            press: 0.,
                        },
                    )
                })?;
                let pixels = surface.pixels(size)?;
                let background = (palette.background.r * tint * 255.).ceil() as u8;
                let left = ((width - 14. - time_width) * scale).floor() as usize;
                let right = ((width - 14.) * scale).ceil() as usize;
                let clock_ink = pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .enumerate()
                    .filter(|(index, pixel)| {
                        let x = index % size.width as usize;
                        x >= left && x < right && pixel[2] > background.saturating_add(3)
                    })
                    .count();
                assert!(
                    clock_ink > 4,
                    "missing clock glyphs: dpi={dpi} reveal={reveal} tint={tint}"
                );
                assert_eq!(pixels[3], 0, "rounded exterior remains transparent");
                assert!(
                    pixels
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .all(|pixel| pixel[..3].iter().all(|channel| *channel <= pixel[3]))
                );
            }
        }
        Ok(())
    }
    fn save_preview(surface: &Surface, path: &std::path::Path) -> std::io::Result<()> {
        let size = surface.target.size;
        let pixels = surface
            .pixels(size)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let length = pixels.len();
        let mut bmp = Vec::with_capacity(54 + length);
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&((54 + length) as u32).to_le_bytes());
        bmp.extend_from_slice(&[0; 4]);
        bmp.extend_from_slice(&54u32.to_le_bytes());
        bmp.extend_from_slice(&40u32.to_le_bytes());
        bmp.extend_from_slice(&(size.width as i32).to_le_bytes());
        bmp.extend_from_slice(&(-(size.height as i32)).to_le_bytes());
        bmp.extend_from_slice(&1u16.to_le_bytes());
        bmp.extend_from_slice(&32u16.to_le_bytes());
        bmp.extend_from_slice(&[0; 24]);
        for p in pixels.as_chunks::<4>().0 {
            let alpha = p[3] as u32;
            let index = bmp.len().saturating_sub(54) / 4;
            let background =
                if ((index % size.width as usize) / 12 + (index / size.width as usize) / 12).is_multiple_of(2) {
                    86
                } else {
                    130
                };
            for c in &p[..3] {
                bmp.push((*c as u32 + background * (255 - alpha) / 255).min(255) as u8);
            }
            bmp.push(255);
        }
        std::fs::write(path, bmp)
    }
    #[test]
    fn cached_popup_reuses_pixels_until_content_changes() -> Result<()> {
        let renderer = Renderer::new()?;
        let palette = Palette::new(true, false);
        let mut surface = Surface::new(&renderer.factory, &palette)?;
        let size = D2D_SIZE_U { width: 120, height: 80 };
        let draws = std::cell::Cell::new(0);
        let presentations = std::cell::Cell::new(0);
        let draw = |target: &mut Target, color: &D2D1_COLOR_F| {
            draws.set(draws.get() + 1);
            unsafe {
                target.rt.Clear(Some(color));
            }
            Ok(())
        };
        surface.prepare(size, 96, (Contour::Round(8.), 255), |t| draw(t, &palette.background))?;
        let original = surface.pixels(size)?.to_vec();
        assert_eq!(original[3], 0, "rounded exterior remains transparent");
        for _ in 0..5 {
            surface.present_prepared(size, 96, || {
                presentations.set(presentations.get() + 1);
                Ok(())
            })?;
            assert_eq!(
                surface.pixels(size)?,
                original,
                "opacity ticks cannot multiply the corner mask"
            );
        }
        assert_eq!(draws.get(), 1);
        assert_eq!(presentations.get(), 5);
        // A content/theme update replaces the complete cached frame at its current alpha.
        surface.prepare(size, 96, (Contour::Round(8.), 255), |t| draw(t, &palette.text))?;
        let updated = surface.pixels(size)?.to_vec();
        assert_ne!(original, updated);
        surface.present_prepared(size, 96, || Ok(()))?;
        assert_eq!(surface.pixels(size)?, updated);
        assert_eq!(draws.get(), 2);
        Ok(())
    }
    #[test]
    fn prepared_frame_rejects_resize_dpi_and_failed_redraw_then_recovers() -> Result<()> {
        let renderer = Renderer::new()?;
        let palette = Palette::new(true, false);
        let mut surface = Surface::new(&renderer.factory, &palette)?;
        let old = D2D_SIZE_U { width: 80, height: 64 };
        let next = D2D_SIZE_U { width: 120, height: 96 };
        let called = std::cell::Cell::new(false);
        let present = || {
            called.set(true);
            Ok(())
        };
        assert!(surface.present_prepared(old, 96, present).is_err());
        surface.prepare(old, 96, (Contour::Round(8.), 255), |_| Ok(()))?;
        assert!(surface.present_prepared(next, 96, present).is_err());
        assert!(surface.present_prepared(old, 144, present).is_err());
        assert!(!called.get());
        surface.prepare(next, 144, (Contour::Round(8.), 255), |_| Ok(()))?;
        surface.present_prepared(next, 144, present)?;
        assert!(called.replace(false));
        assert!(
            surface
                .prepare(next, 144, (Contour::Round(8.), 255), |_| {
                    Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))
                })
                .is_err()
        );
        assert!(surface.present_prepared(next, 144, present).is_err());
        assert!(!called.get(), "failed drawing cannot present stale content");
        surface.prepare(next, 144, (Contour::Round(8.), 255), |_| Ok(()))?;
        surface.present_prepared(next, 144, present)?;
        assert!(called.get());
        Ok(())
    }
    #[test]
    fn failed_shrink_bind_rejects_old_and_new_slices_then_recovers_without_target_recreation() -> Result<()> {
        let renderer = Renderer::new()?;
        let palette = Palette::new(true, false);
        let mut surface = Surface::new(&renderer.factory, &palette)?;
        let old = D2D_SIZE_U { width: 128, height: 64 };
        let small = D2D_SIZE_U { width: 16, height: 8 };
        let draw = |target: &mut Target| {
            unsafe {
                target.rt.Clear(Some(&palette.background));
            }
            Ok(())
        };
        surface.render(old, 96, draw)?;
        let target = surface.target.rt.as_raw();
        let drawn = std::cell::Cell::new(false);
        let error = surface.render_with_bind(
            small,
            96,
            |_, _, _| Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL)),
            |_| {
                drawn.set(true);
                Ok(())
            },
        );
        assert!(error.is_err());
        assert!(!drawn.get());
        assert_eq!(surface.buffer.allocation.unwrap().bytes, 16 * 8 * 4);
        assert!(surface.finish(old, 96, (Contour::Rectangle, 255)).is_err());
        assert!(surface.finish(small, 96, (Contour::Rectangle, 255)).is_err());
        assert!(surface.pixels(old).is_err());
        surface.render(old, 96, draw)?;
        assert_eq!(surface.pixels(old)?.len(), 128 * 64 * 4);
        assert_eq!(surface.target.rt.as_raw(), target);
        assert!(
            surface
                .render_with_bind(
                    small,
                    96,
                    |_, _, _| Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL)),
                    draw
                )
                .is_err()
        );
        let allocated = surface.bits;
        surface.render(small, 96, draw)?;
        assert_eq!(surface.bits, allocated, "binding retry must reuse selected allocation");
        assert_eq!(surface.pixels(small)?.len(), 16 * 8 * 4);
        assert_eq!(surface.target.rt.as_raw(), target);
        Ok(())
    }
    #[test]
    fn allocation_and_selection_failures_leave_previous_buffer_and_drop_rejected_candidate() -> Result<()> {
        struct Candidate(std::rc::Rc<std::cell::Cell<usize>>);
        impl Drop for Candidate {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let old = D2D_SIZE_U { width: 128, height: 64 };
        let mut state = BufferState::default();
        state.selected(BufferLayout::new(old)?);
        bind_selected(&mut state, || Ok::<(), ()>(())).unwrap();
        let drops = std::rc::Rc::new(std::cell::Cell::new(0));
        let failed = allocate_selected(|| Err::<Candidate, _>("allocation"), |_| Ok::<(), _>(()));
        assert!(failed.is_err());
        assert_eq!(state.slice_len(old)?, 128 * 64 * 4);
        let failed = allocate_selected(
            || Ok::<_, &str>(Candidate(drops.clone())),
            |_| Err::<(), _>("selection"),
        );
        assert!(failed.is_err());
        assert_eq!(drops.get(), 1);
        assert_eq!(state.slice_len(old)?, 128 * 64 * 4);
        assert!(BufferLayout::new(D2D_SIZE_U { width: 0, height: 20 }).is_err());
        assert!(
            BufferLayout::new(D2D_SIZE_U {
                width: u32::MAX,
                height: u32::MAX
            })
            .is_err()
        );
        Ok(())
    }
    #[test]
    fn transparent_island_preserves_complete_interactive_shape_through_reveal_and_music() -> Result<()> {
        let renderer = Renderer::new()?;
        let mut palette = Palette::new(true, false);
        palette.background.a = 0.;
        let mut status = crate::dock::status::read_clock();
        status.time = "17:31".into();
        status.date = "Thursday, 12 November".into();
        let (date_width, time_width, _) = renderer.measure_topbar(&status.date, &status.time, "")?;
        let time_width = time_width.max(32.);
        let track = crate::commands::media::NotchTrack {
            media: crate::commands::media::State {
                source: "fixture".into(),
                title: "Midnight City".into(),
                artist: "M83".into(),
                app: "Music".into(),
                playing: true,
                timeline: None,
            },
            artwork: None,
        };
        let mut surface = Surface::new(&renderer.factory, &palette)?;
        for bar_height in [24., 28., 32., 48.] {
            for dpi in [96, 120, 144, 192] {
                for reveal in [0., 0.5, 1.] {
                    for (mix, compact, base_width) in [
                        (0., false, date_width + time_width + 38.),
                        (0.5, false, 380.3),
                        (1., false, 500.3),
                        (1., true, 180.3),
                    ] {
                        let scale = dpi as f32 / 96.;
                        let logical_width = base_width * (0.94 + 0.06 * reveal);
                        let size = D2D_SIZE_U {
                            width: (logical_width * scale).round() as u32,
                            height: ((bar_height - 4.) * (0.85 + 0.15 * reveal) * scale).round() as u32,
                        };
                        surface.render(size, dpi, |target| {
                            draw_island_content(
                                target,
                                &renderer.formats,
                                IslandFrame {
                                    dpi,
                                    palette: &palette,
                                    status: &status,
                                    track: if mix > 0. { Some(&track) } else { None },
                                    previous_track: None,
                                    track_fade: 1.,
                                    reveal,
                                    mix,
                                    width: logical_width,
                                    compact,
                                    time_width,
                                    date_width,
                                    hover: None,
                                    hover_level: 0.,
                                    press: 0.,
                                },
                            )
                        })?;
                        surface.finish(size, dpi, (Contour::Island, 255))?;
                        let pixels = unsafe {
                            std::slice::from_raw_parts(surface.bits, (size.width * size.height * 4) as usize)
                        };
                        let radius = island_radius(size.height as f32 / scale) * scale;
                        for y in 0..size.height {
                            for x in 0..size.width {
                                let dx = (radius - (x as f32 + 0.5).min(size.width as f32 - x as f32 - 0.5)).max(0.);
                                let dy = (radius - (y as f32 + 0.5).min(size.height as f32 - y as f32 - 0.5)).max(0.);
                                let alpha = pixels[((y * size.width + x) * 4 + 3) as usize];
                                if dx * dx + dy * dy <= radius * radius {
                                    assert!(
                                        alpha > 0,
                                        "clickthrough at{x},{y}:height{bar_height},dpi{dpi},reveal{reveal},mix{mix}"
                                    );
                                }
                                if (dx * dx + dy * dy).sqrt() > radius + 0.5 {
                                    assert_eq!(alpha, 0, "exterior must staytransparent");
                                }
                            }
                        }
                        let layout = crate::dock::topbar::model::IslandLayout::new(
                            size.width as f32 / scale,
                            compact,
                            time_width,
                        );
                        if mix > 0. {
                            for (left, hit) in layout.controls.into_iter().zip([
                                crate::dock::topbar::model::Hit::Previous,
                                crate::dock::topbar::model::Hit::PlayPause,
                                crate::dock::topbar::model::Hit::Next,
                            ]) {
                                let x = left + layout.button / 2.;
                                assert_eq!(layout.media_hit(x), Some(hit));
                                let physical_x = (x * scale).floor() as u32;
                                assert!(pixels[(((size.height / 2) * size.width + physical_x) * 4 + 3) as usize] > 0);
                            }
                        }
                        let clock_x = ((layout.clock + time_width / 2.) * scale).floor() as u32;
                        assert!(pixels[(((size.height / 2) * size.width + clock_x) * 4 + 3) as usize] > 0);
                        if bar_height == 28. && dpi == 96 && reveal == 1. && !compact && (mix == 0. || mix == 1.) {
                            let directory =
                                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/qa-island-hit");
                            std::fs::create_dir_all(&directory).map_err(|e| {
                                windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e.to_string())
                            })?;
                            save_preview(
                                &surface,
                                &directory.join(if mix == 0. {
                                    "idle-opacity0.bmp"
                                } else {
                                    "music-opacity0.bmp"
                                }),
                            )
                            .map_err(|e| {
                                windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e.to_string())
                            })?;
                        }
                        for (x, y) in [
                            (0, 0),
                            (size.width - 1, 0),
                            (0, size.height - 1),
                            (size.width - 1, size.height - 1),
                        ] {
                            assert_eq!(pixels[((y * size.width + x) * 4 + 3) as usize], 0);
                        }
                        assert!(
                            pixels
                                .as_chunks::<4>()
                                .0
                                .iter()
                                .all(|p| p[..3].iter().all(|c| *c <= p[3]))
                        );
                    }
                }
            }
        }
        Ok(())
    }
    #[test]
    fn island_clock_has_dedicated_smaller_metrics_without_changing_dock_clock() -> Result<()> {
        let renderer = Renderer::new()?;
        assert_eq!(unsafe { renderer.formats.island_clock.GetFontSize() }, 13.);
        assert_eq!(unsafe { renderer.formats.time.GetFontSize() }, 14.);
        let time = "17:31";
        let wide = time.encode_utf16().collect::<Vec<_>>();
        let measure = |format: &IDWriteTextFormat| -> Result<f32> {
            let layout = unsafe { renderer.dw.CreateTextLayout(&wide, format, 1000., 100.) }?;
            let mut metrics = DWRITE_TEXT_METRICS::default();
            unsafe { layout.GetMetrics(&mut metrics) }?;
            Ok(metrics.widthIncludingTrailingWhitespace.ceil() + 1.)
        };
        let (_, clock, _) = renderer.measure_topbar("Thu12Nov", time, "")?;
        assert_eq!(clock, measure(&renderer.formats.island_clock)?);
        assert!(clock < measure(&renderer.formats.time)?);
        Ok(())
    }
    #[test]
    fn calendar_today_and_historical_selection_have_distinct_final_pixels() -> Result<()> {
        let renderer = Renderer::new()?;
        let palette = Palette::new(true, false);
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/qa-calendar-fix");
        std::fs::create_dir_all(&directory)
            .map_err(|e| windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e.to_string()))?;
        for (name, browse, selection, current) in [
            ("current-november-2026", 0, None, true),
            ("browsed-january-2025", -22, None, false),
            (
                "selected-january-8-2025",
                -22,
                Some(crate::dock::calendar_model::Date::new(2025, 1, 8).unwrap()),
                false,
            ),
        ] {
            let (elements, height) = crate::dock::sound::qa_calendar_elements(browse, selection)?;
            let mut surface = Surface::new(&renderer.factory, &palette)?;
            let size = D2D_SIZE_U {
                width: 392,
                height: height.ceil() as u32,
            };
            surface.render(size, 96, |target| {
                draw_control_content(
                    target,
                    &renderer.formats,
                    ControlFrame {
                        palette: &palette,
                        elements: &elements,
                        scroll: 0.,
                        max_scroll: 0.,
                        focus: None,
                        dragging: None,
                        presentation: (1., 1., 1.),
                    },
                )
            })?;
            surface.finish(size, 96, (Contour::Round(18.), 255))?;
            let pixels = unsafe { std::slice::from_raw_parts(surface.bits, (size.width * size.height * 4) as usize) };
            let accent = pixels
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|p| p[0] > 180 && p[2] < 40 && (100..170).contains(&p[1]))
                .count();
            assert_eq!(
                accent > 200,
                current,
                "only full-date today may own the filled blue accent: {name}"
            );
            save_preview(&surface, &directory.join(format!("{name}.bmp")))
                .map_err(|e| windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e.to_string()))?;
        }
        Ok(())
    }
    #[test]
    fn final_presentation_masks_foreground_border_and_fade() -> Result<()> {
        let renderer = Renderer::new()?;
        let palette = Palette::new(true, false);
        let mut surface = Surface::new(&renderer.factory, &palette)?;
        for dpi in [96, 120, 144, 192] {
            for scale in [0.9, 1.] {
                for opacity in [64, 255] {
                    let size = D2D_SIZE_U {
                        width: (320. * scale * dpi as f32 / 96.) as u32,
                        height: (400. * scale * dpi as f32 / 96.) as u32,
                    };
                    surface.render(size, dpi, |target| {
                        unsafe {
                            target.rt.Clear(Some(&D2D1_COLOR_F {
                                r: 0.4,
                                g: 0.2,
                                b: 0.1,
                                a: 1.,
                            }));
                        }
                        Ok(())
                    })?;
                    surface.finish(size, dpi, (Contour::Round(18. * scale), opacity))?;
                    let pixels =
                        unsafe { std::slice::from_raw_parts(surface.bits, (size.width * size.height * 4) as usize) };
                    for (x, y) in [
                        (0, 0),
                        (size.width - 1, 0),
                        (0, size.height - 1),
                        (size.width - 1, size.height - 1),
                    ] {
                        assert_eq!(pixels[((y * size.width + x) * 4 + 3) as usize], 0);
                    }
                    assert_eq!(
                        pixels[(((size.height / 2) * size.width + size.width / 2) * 4 + 3) as usize],
                        opacity
                    );
                    assert!(
                        pixels
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .all(|p| p[..3].iter().all(|c| *c <= p[3]))
                    );
                }
            }
        }
        Ok(())
    }
    #[test]
    fn render_compact_controls_scenarios_at_supported_dpi() -> Result<()> {
        let renderer = Renderer::new()?;
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/qa-compact-controls");
        std::fs::create_dir_all(&directory)
            .map_err(|e| windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e.to_string()))?;
        for dark in [true, false] {
            let palette = Palette::new(dark, false);
            for scenario in ["default", "no-sound", "hidden", "long-labels", "media"] {
                let (elements, height) = crate::dock::sound::qa_compact_elements(scenario)?;
                for dpi in [96, 120, 144, 192] {
                    let scale = dpi as f32 / 96.;
                    let size = D2D_SIZE_U {
                        width: (360. * scale).round() as u32,
                        height: (height * scale).ceil() as u32,
                    };
                    let mut surface = Surface::new(&renderer.factory, &palette)?;
                    surface.render(size, dpi, |target| {
                        draw_control_content(
                            target,
                            &renderer.formats,
                            ControlFrame {
                                palette: &palette,
                                elements: &elements,
                                scroll: 0.,
                                max_scroll: 0.,
                                focus: None,
                                dragging: None,
                                presentation: (1., 1., 1.),
                            },
                        )
                    })?;
                    surface.finish(size, dpi, (Contour::Round(18.), 255))?;
                    let pixels = surface.pixels(size)?;
                    for (x, y) in [
                        (0, 0),
                        (size.width - 1, 0),
                        (0, size.height - 1),
                        (size.width - 1, size.height - 1),
                    ] {
                        assert_eq!(pixels[((y * size.width + x) * 4 + 3) as usize], 0);
                    }
                    assert!(
                        pixels
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .all(|pixel| pixel[..3].iter().all(|c| *c <= pixel[3]))
                    );
                    if dpi == 96 {
                        save_preview(
                            &surface,
                            &directory.join(format!("{scenario}-{}.bmp", if dark { "dark" } else { "light" })),
                        )
                        .map_err(|e| windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e.to_string()))?;
                    }
                }
            }
        }
        Ok(())
    }
    #[test]
    fn render_topbar_calendar_qa_fixtures() -> Result<()> {
        let renderer = Renderer::new()?;
        let palette = Palette::new(true, false);
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/qa-topbar-motion");
        std::fs::create_dir_all(&directory)
            .map_err(|e| windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e.to_string()))?;
        for (name, entry, width, scroll_bottom) in [
            ("wifi", crate::dock::sound::Entry::Wifi, 424., false),
            ("wifi-scrolled", crate::dock::sound::Entry::Wifi, 424., true),
            ("sound", crate::dock::sound::Entry::Sound, 424., false),
            ("calendar", crate::dock::sound::Entry::Calendar, 392., false),
        ] {
            let (elements, height) = crate::dock::sound::qa_elements(entry, width)?;
            let shown_height = height.min(480.);
            let max_scroll = (height - shown_height).max(0.);
            let mut surface = Surface::new(&renderer.factory, &palette)?;
            surface.render(
                D2D_SIZE_U {
                    width: width as u32,
                    height: shown_height.ceil() as u32,
                },
                96,
                |target| {
                    draw_control_content(
                        target,
                        &renderer.formats,
                        ControlFrame {
                            palette: &palette,
                            elements: &elements,
                            scroll: if scroll_bottom { max_scroll } else { 0. },
                            max_scroll,
                            focus: None,
                            dragging: None,
                            presentation: (1., 1., 1.),
                        },
                    )
                },
            )?;
            surface.finish(surface.target.size, 96, (Contour::Round(18.), 255))?;
            save_preview(&surface, &directory.join(format!("{name}.bmp")))
                .map_err(|e| windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e.to_string()))?;
        }
        let mut status = crate::dock::status::read_clock();
        status.time = "17:31".into();
        let (date_width, time_width, _) = renderer.measure_topbar(&status.date, &status.time, "")?;
        let width = date_width + time_width + 38.;
        let mut surface = Surface::new(&renderer.factory, &palette)?;
        surface.render(
            D2D_SIZE_U {
                width: width.ceil() as u32,
                height: 24,
            },
            96,
            |target| {
                draw_island_content(
                    target,
                    &renderer.formats,
                    IslandFrame {
                        dpi: 96,
                        palette: &palette,
                        status: &status,
                        track: None,
                        previous_track: None,
                        track_fade: 1.,
                        reveal: 1.,
                        mix: 0.,
                        width,
                        compact: false,
                        time_width,
                        date_width,
                        hover: None,
                        hover_level: 0.,
                        press: 0.,
                    },
                )
            },
        )?;
        surface.finish(surface.target.size, 96, (Contour::Island, 255))?;
        save_preview(&surface, &directory.join("island-28.bmp"))
            .map_err(|e| windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e.to_string()))?;
        let track = crate::commands::media::NotchTrack {
            media: crate::commands::media::State {
                source: "fixture".into(),
                title: "Midnight City".into(),
                artist: "M83".into(),
                app: "Music".into(),
                playing: true,
                timeline: None,
            },
            artwork: None,
        };
        surface.render(D2D_SIZE_U { width: 500, height: 24 }, 96, |target| {
            draw_island_content(
                target,
                &renderer.formats,
                IslandFrame {
                    dpi: 96,
                    palette: &palette,
                    status: &status,
                    track: Some(&track),
                    previous_track: None,
                    track_fade: 1.,
                    reveal: 1.,
                    mix: 1.,
                    width: 500.,
                    compact: false,
                    time_width,
                    date_width,
                    hover: None,
                    hover_level: 0.,
                    press: 0.,
                },
            )
        })?;
        surface.finish(surface.target.size, 96, (Contour::Island, 255))?;
        save_preview(&surface, &directory.join("island-music-28.bmp"))
            .map_err(|e| windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e.to_string()))?;
        surface.render(D2D_SIZE_U { width: 380, height: 24 }, 96, |target| {
            draw_island_content(
                target,
                &renderer.formats,
                IslandFrame {
                    dpi: 96,
                    palette: &palette,
                    status: &status,
                    track: Some(&track),
                    previous_track: None,
                    track_fade: 1.,
                    reveal: 1.,
                    mix: 0.5,
                    width: 380.,
                    compact: false,
                    time_width,
                    date_width,
                    hover: None,
                    hover_level: 0.,
                    press: 0.,
                },
            )
        })?;
        surface.finish(surface.target.size, 96, (Contour::Island, 255))?;
        save_preview(&surface, &directory.join("island-transition.bmp"))
            .map_err(|e| windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e.to_string()))?;
        let shown = (0..5)
            .map(|uid| {
                let mut bgra = vec![0; 16 * 16 * 4];
                for y in 0usize..16 {
                    for x in 0usize..16 {
                        if (x as i32 - 8).pow(2) + (y as i32 - 8).pow(2) < 45 {
                            let offset = (y * 16 + x) * 4;
                            bgra[offset..offset + 4].copy_from_slice(&[
                                160 + uid as u8 * 15,
                                160 + uid as u8 * 15,
                                160 + uid as u8 * 15,
                                255,
                            ]);
                        }
                    }
                }
                Shown {
                    icon: crate::tray_host::Icon {
                        hwnd: 1,
                        uid,
                        guid: None,
                        callback: 1,
                        version: 4,
                        state: 0,
                        tip: format!("Fixture app {uid}"),
                        image: None,
                        revision: 1,
                        pid: 0,
                        exes: Vec::new(),
                        promoted: None,
                        order: None,
                        accepted: true,
                    },
                    pixels: Some(Arc::new(IconPixels {
                        width: 16,
                        height: 16,
                        bgra,
                    })),
                }
            })
            .collect::<Vec<_>>();
        for (name, progress) in [("tray-opening", 0.4), ("tray-open", 1.), ("tray-closing", 0.2)] {
            let visual = crate::dock::topbar::model::surface_visual(progress);
            let size = D2D_SIZE_U {
                width: (216. * visual.scale).round() as u32,
                height: (56. * visual.scale).round() as u32,
            };
            surface.render(size, 96, |target| {
                draw_topbar_overflow_content(
                    target,
                    &TopbarFlyoutFrame {
                        dpi: 96,
                        palette: &palette,
                        shown: &shown,
                        overflow: &[0, 1, 2, 3, 4],
                        hover: None,
                        presentation: (visual.scale, visual.opacity),
                    },
                )
            })?;
            surface.finish(
                size,
                96,
                (
                    Contour::Round(12. * visual.scale),
                    (visual.opacity * 255.).round() as u8,
                ),
            )?;
            let pixels = unsafe { std::slice::from_raw_parts(surface.bits, (size.width * size.height * 4) as usize) };
            assert_eq!(pixels[3], 0);
            assert_eq!(
                pixels[(((size.height / 2) * size.width + size.width / 2) * 4 + 3) as usize],
                (progress * 255.).round() as u8
            );
            save_preview(&surface, &directory.join(format!("{name}.bmp")))
                .map_err(|e| windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e.to_string()))?;
        }
        Ok(())
    }
    #[test]
    fn motion_frames_reuse_both_artworks_and_have_bounded_icon_cache() -> Result<()> {
        let renderer = Renderer::new()?;
        let palette = Palette::new(true, false);
        let status = crate::dock::status::read_clock();
        let track = |title: &str, pixel: u8| crate::commands::media::NotchTrack {
            media: crate::commands::media::State {
                source: "fixture".into(),
                title: title.into(),
                artist: "Artist".into(),
                app: "Music".into(),
                playing: true,
                timeline: None,
            },
            artwork: Some(Arc::new(IconPixels {
                width: 2,
                height: 2,
                bgra: [pixel, pixel, pixel, 255].repeat(4),
            })),
        };
        let old = track("Previous song", 100);
        let new = track("Next song", 200);
        let mut surface = Surface::new(&renderer.factory, &palette)?;
        let render = |surface: &mut Surface, previous, fade| {
            surface.render(D2D_SIZE_U { width: 500, height: 24 }, 96, |target| {
                draw_island_content(
                    target,
                    &renderer.formats,
                    IslandFrame {
                        dpi: 96,
                        palette: &palette,
                        status: &status,
                        track: Some(&new),
                        previous_track: previous,
                        track_fade: fade,
                        reveal: 1.,
                        mix: 1.,
                        width: 500.,
                        compact: false,
                        time_width: 48.,
                        date_width: 120.,
                        hover: None,
                        hover_level: 0.,
                        press: 0.,
                    },
                )
            })
        };
        render(&mut surface, Some(&old), 0.4)?;
        let before = surface
            .target
            .bitmaps
            .iter()
            .map(|(key, (_, bitmap))| (key.clone(), bitmap.as_raw() as usize))
            .collect::<HashMap<_, _>>();
        assert_eq!(before.len(), 2);
        render(&mut surface, Some(&old), 0.6)?;
        assert!(
            surface
                .target
                .bitmaps
                .iter()
                .all(|(key, (_, bitmap))| before[key] == bitmap.as_raw() as usize)
        );
        assert_eq!(surface.target.media_shapes.len(), 2);
        render(&mut surface, None, 1.)?;
        assert_eq!(surface.target.bitmaps.len(), 1);
        Ok(())
    }
    #[test]
    fn island_shape_masks_all_four_corners_at_every_size_and_dpi() -> Result<()> {
        unsafe {
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let palette = Palette::new(true, false);
            let mut surface = Surface::new(&factory, &palette)?;
            for height in [24., 28., 32., 48.] {
                for dpi in [96, 120, 144, 192] {
                    for reveal in [0.85, 1.] {
                        let scale = dpi as f32 / 96.;
                        let size = D2D_SIZE_U {
                            width: (220. * scale).round() as u32,
                            height: ((height - 4.) * reveal * scale).round() as u32,
                        };
                        surface.render(size, dpi, |target| {
                            island_background(target, &palette);
                            Ok(())
                        })?;
                        surface.finish(size, dpi, (Contour::Island, 255))?;
                        let pixels = std::slice::from_raw_parts(surface.bits, (size.width * size.height * 4) as usize);
                        for (x, y) in [
                            (0, 0),
                            (size.width - 1, 0),
                            (0, size.height - 1),
                            (size.width - 1, size.height - 1),
                        ] {
                            assert_eq!(
                                pixels[((y * size.width + x) * 4 + 3) as usize],
                                0,
                                "square capsule corner at {height} DIP/{dpi} DPI/reveal{reveal}"
                            );
                        }
                        // A restrained corner leaves a flat shoulder near the edge;
                        // a full capsule would erase this top-row point on tall bars.
                        let shoulder = (island_radius(size.height as f32 / scale) * scale).ceil() as u32 + 2;
                        assert!(pixels[(shoulder * 4 + 3) as usize] > 240);
                        assert_eq!(
                            pixels[(((size.height / 2) * size.width + size.width / 2) * 4 + 3) as usize],
                            255
                        );
                    }
                }
            }
            Ok(())
        }
    }
    #[test]
    fn offscreen_pixels_preserve_zero_alpha_and_complete_hit_cells() -> Result<()> {
        unsafe {
            // No HWND, desktop access or visible application is created by this raster test.
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let palette = Palette::new(true, true);
            let mut surface = Surface::new(&factory, &palette)?;
            surface.render(D2D_SIZE_U { width: 128, height: 28 }, 96, |target| {
                target.rt.Clear(Some(&D2D1_COLOR_F {
                    r: 0.,
                    g: 0.,
                    b: 0.,
                    a: 0.,
                }));
                hit_backdrop(target, rect(12., 0., 40., 28.));
                target.brush.SetColor(&D2D1_COLOR_F {
                    r: 1.,
                    g: 1.,
                    b: 1.,
                    a: 1.,
                });
                target.rt.FillRectangle(&rect(24., 10., 28., 18.), &target.brush);
                Ok(())
            })?;
            let pixels = std::slice::from_raw_parts(surface.bits, 128 * 28 * 4);
            let pixel = |x: usize, y: usize| &pixels[(y * 128 + x) * 4..(y * 128 + x) * 4 + 4];
            assert_eq!(pixel(0, 0), [0, 0, 0, 0]);
            assert_eq!(pixel(100, 14), [0, 0, 0, 0]);
            for (x, y) in [(12, 0), (39, 0), (12, 27), (39, 27)] {
                assert_eq!(pixel(x, y)[3], 1, "hit cell corner must receive input");
            }
            assert_eq!(pixel(25, 12), [255, 255, 255, 255]);
            for p in pixels.as_chunks::<4>().0 {
                assert!(p[..3].iter().all(|c| *c <= p[3]));
            }
            surface.render(D2D_SIZE_U { width: 192, height: 42 }, 144, |target| {
                target.rt.Clear(Some(&D2D1_COLOR_F {
                    r: 0.,
                    g: 0.,
                    b: 0.,
                    a: 0.,
                }));
                hit_backdrop(target, rect(12., 0., 40., 28.));
                Ok(())
            })?;
            let resized = std::slice::from_raw_parts(surface.bits, 192 * 42 * 4);
            assert_eq!(resized[(41 * 192 + 18) * 4 + 3], 1);
            assert_eq!(resized[(41 * 192 + 59) * 4 + 3], 1);
            assert_eq!(resized[(20 * 192 + 100) * 4 + 3], 0);
            Ok(())
        }
    }
}
