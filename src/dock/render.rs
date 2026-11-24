//! Small Direct2D renderer, separate from the launcher's renderer.

use super::logic::Part;
use super::status::Status;
use super::topbar::model::{EQUALIZER_BAR_COUNT, EQUALIZER_BAR_STEP, EQUALIZER_BAR_WIDTH};
use super::tray::Shown;
use super::{Item, logic};
use crate::dock_gradient::{Direction, Gradient};
use crate::indexer::IconPixels;
use crate::theme::Palette;
use crate::tray_host::Key;
use std::collections::HashMap;
use std::sync::Arc;
use windows::Win32::Foundation::{D2DERR_RECREATE_TARGET, HWND, RECT};
use windows::Win32::Graphics::Direct2D::{Common::*, *};
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, GetForegroundWindow};
use windows::core::{Interface, PCWSTR, Result, w};
#[path = "render_layered.rs"]
mod layered;
use windows_numerics::Matrix3x2;

/// Vertical layout, in DIPs, shared by the app slots and the status parts.
/// Icons sit a little above center to leave room for the running indicator.
use super::logic::{ICON_CENTER_Y, INDICATOR_TOP, TILE_BOTTOM, TILE_RADIUS, TILE_TOP};
const UPPER: (f32, f32) = (8.0, 29.0);
const LOWER: (f32, f32) = (28.0, 48.0);
/// Shared by top-bar tray foreground pixels, border and native blur clip, in DIPs.
pub(super) const TOPBAR_FLYOUT_RADIUS: f32 = 12.;

struct Target {
    rt: ID2D1RenderTarget,
    window: Option<ID2D1HwndRenderTarget>,
    brush: ID2D1SolidColorBrush,
    bitmaps: HashMap<String, (Arc<IconPixels>, ID2D1Bitmap)>,
    /// Notification-area icons, by their tray identity.
    tray: HashMap<Key, (Arc<IconPixels>, ID2D1Bitmap)>,
    media_shapes: HashMap<u8, ID2D1PathGeometry>,
    launcher_shape: Option<ID2D1PathGeometry>,
    dock_gradient: Option<(Gradient, ID2D1LinearGradientBrush)>,
    size: D2D_SIZE_U,
}

#[derive(Clone)]
struct Formats {
    /// A fallback tile's initial.
    letter: IDWriteTextFormat,
    time: IDWriteTextFormat,
    island_clock: IDWriteTextFormat,
    small: IDWriteTextFormat,
    /// Segoe Fluent Icons for the status glyphs.
    glyph: IDWriteTextFormat,
    chevron: IDWriteTextFormat,
    /// Tooltips: left-aligned and wrapping.
    tip: IDWriteTextFormat,
    /// One line of the window list, clipped at the card's edge.
    row: IDWriteTextFormat,
    control_title: IDWriteTextFormat,
    control_detail: IDWriteTextFormat,
    heading_leading: IDWriteTextFormat,
}

pub struct Renderer {
    factory: ID2D1Factory,
    dw: IDWriteFactory,
    formats: Formats,
    text_font: PCWSTR,
    display_font: PCWSTR,
    icon_font: PCWSTR,
    format_scale: f32,
    target: Option<Target>,
    shared_target: Option<Target>,
    layered: Option<layered::Surface>,
}

pub(super) struct DragFrame {
    pub from: usize,
    pub to: Option<usize>,
    pub offset: f32,
}

pub struct Frame<'a> {
    pub animation_scale: f32,
    pub gradient: Option<Gradient>,
    pub alpha: u8,
    pub dpi: u32,
    pub palette: &'a Palette,
    pub(super) items: &'a [Item],
    pub icons: &'a HashMap<String, Arc<IconPixels>>,
    pub hover: Option<usize>,
    pub hover_levels: &'a [f32],
    pub app_icon_size: f32,
    pub layout_scale: f32,
    pub capacity: usize,
    pub scroll: usize,
    pub status: &'a Status,
    pub hover_part: Option<Part>,
    pub parts: &'a [Part],
    /// Every shown notification icon, and which of them sit in the dock.
    pub tray: &'a [Shown],
    pub inline: &'a [usize],
    pub flyout_open: bool,
    pub sound_open: bool,
    pub(super) drag: Option<DragFrame>,
}

/// The notification-area flyout's grid.
pub struct FlyoutFrame<'a> {
    pub dpi: u32,
    pub palette: &'a Palette,
    pub tray: &'a [Shown],
    pub hover: Option<usize>,
}

pub(super) struct TopbarFrame<'a> {
    pub dpi: u32,
    pub palette: &'a Palette,
    pub layout: super::topbar::model::Layout,
    pub shown: &'a [Shown],
    pub inline: &'a [usize],
    pub status: &'a Status,
    pub commands: &'a crate::commands::State,
    pub hover: Option<super::topbar::model::Hit>,
    pub hover_level: f32,
    pub drag_target: Option<bool>,
}
pub(super) struct TopbarFlyoutFrame<'a> {
    pub dpi: u32,
    pub palette: &'a Palette,
    pub shown: &'a [Shown],
    pub overflow: &'a [usize],
    pub hover: Option<usize>,
    pub presentation: (f32, f32),
}
impl TopbarFlyoutFrame<'_> {
    fn cached_contour(&self) -> (layered::Contour, u8) {
        // The retained DIB owns complete pixels. Only native presentation fades it.
        (layered::Contour::Round(TOPBAR_FLYOUT_RADIUS * self.presentation.0), 255)
    }
}
pub(super) struct IslandFrame<'a> {
    pub dpi: u32,
    pub palette: &'a Palette,
    pub status: &'a Status,
    pub track: Option<&'a crate::commands::media::NotchTrack>,
    pub previous_track: Option<&'a crate::commands::media::NotchTrack>,
    pub track_fade: f32,
    pub show_title: bool,
    pub show_previous_title: bool,
    pub equalizer: [f32; EQUALIZER_BAR_COUNT],
    pub equalizer_color: D2D1_COLOR_F,
    pub reveal: f32,
    pub mix: f32,
    pub width: f32,
    pub compact: bool,
    pub time_width: f32,
    pub date_width: f32,
    pub hover: Option<super::topbar::model::Hit>,
    pub hover_level: f32,
    pub press: f32,
}

fn create_formats(dw: &IDWriteFactory, text: PCWSTR, display: PCWSTR, icons: PCWSTR, scale: f32) -> Result<Formats> {
    unsafe {
        let format = |family: PCWSTR, weight, size| -> Result<IDWriteTextFormat> {
            let format = dw.CreateTextFormat(
                family,
                None,
                weight,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size * scale,
                w!("en-us"),
            )?;
            format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            Ok(format)
        };
        let tip = dw.CreateTextFormat(
            text,
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            12.0,
            w!("en-us"),
        )?;
        tip.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
        tip.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR)?;
        tip.SetWordWrapping(DWRITE_WORD_WRAPPING_WRAP)?;
        let row = dw.CreateTextFormat(
            text,
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            11.5 * scale,
            w!("en-us"),
        )?;
        row.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
        let control_title = format(text, DWRITE_FONT_WEIGHT_SEMI_BOLD, 14.0)?;
        control_title.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
        let control_detail = format(text, DWRITE_FONT_WEIGHT_NORMAL, 12.5)?;
        control_detail.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
        let trimming = DWRITE_TRIMMING {
            granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
            delimiter: 0,
            delimiterCount: 0,
        };
        for format in [&control_title, &control_detail] {
            let ellipsis = dw.CreateEllipsisTrimmingSign(format)?;
            format.SetTrimming(&trimming, &ellipsis)?;
        }
        let heading_leading = format(text, DWRITE_FONT_WEIGHT_SEMI_BOLD, 18.)?;
        heading_leading.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
        Ok(Formats {
            letter: format(display, DWRITE_FONT_WEIGHT_SEMI_BOLD, 15.0)?,
            time: format(text, DWRITE_FONT_WEIGHT_SEMI_BOLD, 14.0)?,
            island_clock: format(text, DWRITE_FONT_WEIGHT_NORMAL, 13.0)?,
            small: format(text, DWRITE_FONT_WEIGHT_NORMAL, 12.5)?,
            glyph: format(icons, DWRITE_FONT_WEIGHT_NORMAL, 17.0)?,
            chevron: format(icons, DWRITE_FONT_WEIGHT_NORMAL, 12.0)?,
            tip,
            row,
            control_title,
            control_detail,
            heading_leading,
        })
    }
}

impl Frame<'_> {
    fn count(&self) -> usize {
        self.items.len().saturating_sub(self.scroll).min(self.capacity)
    }

    /// Rounds a DIP coordinate to the nearest physical pixel, for crisp edges.
    fn snap(&self, dip: f32) -> f32 {
        let scale = self.dpi as f32 / 96.0;
        (dip * scale).round() / scale
    }

    fn snap_rect(&self, left: f32, top: f32, right: f32, bottom: f32) -> D2D_RECT_F {
        rect(self.snap(left), self.snap(top), self.snap(right), self.snap(bottom))
    }

    /// One physical pixel, in DIPs.
    fn hairline(&self) -> f32 {
        96.0 / self.dpi as f32
    }
}

impl Renderer {
    pub(super) fn measure_topbar(&self, date: &str, time: &str, title: &str) -> Result<(f32, f32, f32)> {
        let measure = |value: &str, format: &IDWriteTextFormat| -> Result<f32> {
            let wide: Vec<u16> = value.encode_utf16().collect();
            let layout = unsafe { self.dw.CreateTextLayout(&wide, format, 2000., 48.) }?;
            let mut metrics = DWRITE_TEXT_METRICS::default();
            unsafe {
                layout.GetMetrics(&mut metrics)?;
            }
            Ok(metrics.widthIncludingTrailingWhitespace.ceil() + 1.)
        };
        Ok((
            measure(date, &self.formats.small)?,
            measure(time, &self.formats.island_clock)?,
            measure(title, &self.formats.control_detail)?,
        ))
    }
    pub(super) fn draw_topbar(&mut self, hwnd: HWND, frame: TopbarFrame) -> Result<()> {
        let TopbarFrame {
            dpi,
            palette,
            layout,
            shown,
            inline,
            status,
            commands,
            hover,
            hover_level,
            drag_target,
        } = frame;
        use super::topbar::model::Hit;
        self.update_formats(1.)?;
        self.paint_layered(hwnd, dpi, palette, |target, formats| {
            let rt = target.rt.clone();
            let brush = target.brush.clone();
            unsafe {
                rt.Clear(Some(&palette.background));
            }
            target.tray.retain(|key, _| shown.iter().any(|s| s.icon.key() == *key));
            for (i, &index) in inline.iter().enumerate() {
                let left = 12. + i as f32 * 28.;
                hit_backdrop(target, rect(left, 0., left + 28., layout.height));
                if hover == Some(Hit::Tray(i)) {
                    highlight(target, palette, left, 28., layout.height, hover_level);
                }
                if let Some(icon) = shown.get(index) {
                    tray_icon(target, dpi, icon, left + 14., layout.height / 2.)?;
                }
            }
            let start = layout.right();
            let mut controls = vec![
                (
                    Hit::Overflow,
                    12. + inline.len() as f32 * 28.,
                    28.,
                    "\u{E70D}".to_owned(),
                ),
                (Hit::Wifi, start, 32., crate::commands::glyph::WIFI.to_owned()),
                (
                    Hit::Sound,
                    start + 32.,
                    32.,
                    if commands
                        .audio
                        .as_ref()
                        .and_then(|v| v.as_ref())
                        .is_some_and(|a| a.muted)
                    {
                        crate::commands::glyph::MUTED
                    } else {
                        crate::commands::glyph::VOLUME
                    }
                    .to_owned(),
                ),
                (Hit::Controls, layout.width - 52., 40., "\u{E9E9}".to_owned()),
            ];
            if let Some(battery) = status.battery {
                controls.push((Hit::Battery, start + 64., 48., format!("{}%", battery.percent)));
            }
            for (hit, left, width, label) in controls {
                hit_backdrop(target, rect(left, 0., left + width, layout.height));
                if hover == Some(hit) {
                    highlight(target, palette, left, width, layout.height, hover_level);
                }
                unsafe {
                    brush.SetColor(&palette.text);
                }
                text(
                    &rt,
                    &brush,
                    if hit == Hit::Battery {
                        &formats.small
                    } else {
                        &formats.glyph
                    },
                    &label,
                    rect(left, 0., left + width, layout.height),
                );
            }
            if let Some(target_inline) = drag_target {
                let right = (44. + inline.len() as f32 * 28.).max(128.);
                hit_backdrop(target, rect(8., 0., right, layout.height));
                if target_inline {
                    unsafe {
                        brush.SetColor(&palette.accent);
                        rt.DrawRoundedRectangle(
                            &rounded(rect(8., 2., right, layout.height - 2.), 5.),
                            &brush,
                            1.5,
                            None,
                        );
                    }
                }
            }
            Ok(())
        })
    }

    pub(super) fn draw_topbar_island(&mut self, hwnd: HWND, frame: IslandFrame) -> Result<()> {
        let dpi = frame.dpi;
        let palette = frame.palette;
        self.update_formats(1.)?;
        self.paint_surface(
            hwnd,
            dpi,
            palette,
            (layered::Contour::Island, 255),
            |target, formats| draw_island_content(target, formats, frame),
        )
    }

    pub(super) fn draw_topbar_flyout(&mut self, hwnd: HWND, frame: TopbarFlyoutFrame) -> Result<()> {
        let p = frame.presentation;
        self.paint_surface_with_alpha(
            hwnd,
            frame.dpi,
            frame.palette,
            frame.cached_contour(),
            (p.1.clamp(0., 1.) * 255.).round() as u8,
            |target, _| draw_topbar_overflow_content(target, &frame),
        )
    }
    pub(super) fn clear_media_artwork(&mut self) {
        self.layered = None;
        if let Some(target) = &mut self.target {
            target.bitmaps.clear();
        }
    }
    pub fn new() -> Result<Self> {
        unsafe {
            let factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let fonts = crate::render::Fonts::installed(&dw);
            let formats = create_formats(&dw, fonts.text, fonts.display, fonts.icons, 1.0)?;
            Ok(Self {
                factory,
                formats,
                text_font: fonts.text,
                display_font: fonts.display,
                icon_font: fonts.icons,
                format_scale: 1.0,
                dw,
                target: None,
                shared_target: None,
                layered: None,
            })
        }
    }

    /// Reuses the factories and text formats, with a fresh target for another HWND.
    pub fn for_window(&self) -> Self {
        Self {
            factory: self.factory.clone(),
            dw: self.dw.clone(),
            formats: self.formats.clone(),
            text_font: self.text_font,
            display_font: self.display_font,
            icon_font: self.icon_font,
            format_scale: self.format_scale,
            target: None,
            shared_target: None,
            layered: None,
        }
    }

    pub fn draw(&mut self, hwnd: HWND, frame: &Frame) -> Result<()> {
        self.update_formats(frame.layout_scale)?;
        if let Some(gradient) = frame.gradient {
            return self.paint_surface_with_alpha(
                hwnd,
                frame.dpi,
                frame.palette,
                (
                    layered::Contour::Round(8.0 * frame.layout_scale * frame.animation_scale),
                    255,
                ),
                frame.alpha,
                |target, formats| {
                    paint_dock_gradient(
                        target,
                        frame.palette,
                        8.0 * frame.layout_scale * frame.animation_scale,
                        gradient,
                    )?;
                    draw_content(target, formats, frame)
                },
            );
        }
        self.paint(hwnd, frame.dpi, frame.palette, |target, formats| {
            background(target, frame.palette, frame.layout_scale);
            draw_content(target, formats, frame)
        })
    }

    pub(super) fn present_cached_dock(&self, hwnd: HWND, dpi: u32, alpha: u8) -> Result<bool> {
        if let Some(surface) = &self.layered
            && surface.cached_matches(hwnd, dpi)?
        {
            surface.present_cached(hwnd, dpi, alpha)?;
            return Ok(true);
        }
        Ok(false)
    }

    pub(super) fn clear_dock_target(&mut self) {
        self.target = None;
        self.layered = None;
    }

    pub fn draw_on(&mut self, rt: &ID2D1HwndRenderTarget, frame: &Frame) -> Result<()> {
        self.update_formats(frame.layout_scale)?;
        let base: ID2D1RenderTarget = rt.cast()?;
        if self.shared_target.as_ref().is_none_or(|t| t.rt != base) {
            self.shared_target = Some(Target {
                rt: base,
                window: None,
                brush: unsafe { rt.CreateSolidColorBrush(&frame.palette.text, None)? },
                bitmaps: HashMap::new(),
                tray: HashMap::new(),
                media_shapes: HashMap::new(),
                launcher_shape: None,
                dock_gradient: None,
                size: D2D_SIZE_U::default(),
            });
        }
        let target = self.shared_target.as_mut().unwrap();
        draw_content(target, &self.formats, frame)
    }

    fn update_formats(&mut self, scale: f32) -> Result<()> {
        if self.format_scale != scale {
            self.formats = create_formats(&self.dw, self.text_font, self.display_font, self.icon_font, scale)?;
            self.format_scale = scale;
        }
        Ok(())
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "Draws from borrowed control content and per-frame input state"
    )]
    pub fn draw_control(
        &mut self,
        hwnd: HWND,
        dpi: u32,
        palette: &Palette,
        elements: &[super::sound::Element],
        scroll: f32,
        max_scroll: f32,
        focus: Option<usize>,
        dragging: Option<(usize, u32)>,
        presentation: (f32, f32, f32),
    ) -> Result<()> {
        self.paint_surface(
            hwnd,
            dpi,
            palette,
            (
                layered::Contour::Round(18. * presentation.1),
                (presentation.2.clamp(0., 1.) * 255.).round() as u8,
            ),
            |target, formats| {
                draw_control_content(
                    target,
                    formats,
                    ControlFrame {
                        palette,
                        elements,
                        scroll,
                        max_scroll,
                        focus,
                        dragging,
                        presentation,
                    },
                )
            },
        )
    }

    /// Start's immutable eight-action popup shares only the retained layered
    /// surface; Direct2D targets and font formats remain renderer-private.
    pub(crate) fn draw_app_menu(
        &mut self,
        hwnd: HWND,
        dpi: u32,
        palette: &Palette,
        model: &crate::app::start::app_menu::Model,
        alpha: u8,
    ) -> Result<()> {
        use crate::app::start::app_menu::{PAD, RADIUS};
        self.paint_surface_with_alpha(
            hwnd,
            dpi,
            palette,
            (layered::Contour::Round(RADIUS), 255),
            alpha,
            |target, formats| {
                crate::theme::paint_background(&target.rt, &target.brush, palette, RADIUS);
                let width = unsafe { target.rt.GetSize() }.width;
                let rt = &target.rt;
                let brush = &target.brush;
                unsafe {
                    rt.PushAxisAlignedClip(
                        &rect(PAD, PAD, width - PAD, model.view - PAD),
                        D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                    );
                    for (index, row) in model.rows.iter().enumerate() {
                        let top = model.top(index);
                        let bottom = top + row.height();
                        if bottom <= PAD || top >= model.view - PAD {
                            continue;
                        }
                        if model.focus == Some(index) && row.enabled() {
                            brush.SetColor(&palette.row_selected);
                            rt.FillRoundedRectangle(&rounded(rect(PAD, top, width - PAD, bottom), RADIUS - PAD), brush);
                            brush.SetColor(&palette.divider);
                            rt.DrawRoundedRectangle(
                                &rounded(
                                    rect(PAD + 0.5, top + 0.5, width - PAD - 0.5, bottom - 0.5),
                                    RADIUS - PAD,
                                ),
                                brush,
                                1.,
                                None,
                            );
                        }
                        brush.SetColor(if row.enabled() {
                            &palette.subtext
                        } else {
                            &palette.placeholder
                        });
                        text(
                            rt,
                            brush,
                            &formats.glyph,
                            row.glyph,
                            rect(16., top + 1., 34., top + 33.),
                        );
                        brush.SetColor(if row.enabled() {
                            &palette.text
                        } else {
                            &palette.placeholder
                        });
                        let label_bottom = if row.reason.is_some() { top + 27. } else { bottom };
                        text(
                            rt,
                            brush,
                            &formats.control_detail,
                            row.label,
                            rect(44., top, width - 16., label_bottom),
                        );
                        if let Some(reason) = row.reason {
                            brush.SetColor(&palette.subtext);
                            text(
                                rt,
                                brush,
                                &formats.row,
                                reason,
                                rect(44., top + 25., width - 16., bottom - 2.),
                            );
                        }
                    }
                    rt.PopAxisAlignedClip();
                    if model.max_scroll() > 0. {
                        let rail = model.view - 2. * PAD;
                        let thumb = (rail * model.view / (model.view + model.max_scroll()))
                            .max(20.)
                            .min(rail);
                        let top = PAD + (rail - thumb) * model.scroll / model.max_scroll();
                        brush.SetColor(&palette.scrollbar);
                        rt.FillRoundedRectangle(&rounded(rect(width - 5., top, width - 3., top + thumb), 1.), brush);
                    }
                }
                Ok(())
            },
        )
    }

    pub(crate) fn draw_notch(
        &mut self,
        hwnd: HWND,
        dpi: u32,
        palette: &Palette,
        track: &crate::commands::media::NotchTrack,
        progress: f32,
        hover: Option<(f32, f32)>,
    ) -> Result<()> {
        use crate::commands::glyph;
        self.paint(hwnd, dpi, palette, |target, formats| {
            let rt = &target.rt;
            let brush = &target.brush;
            let width = unsafe { rt.GetSize() }.width;
            let lerp = |a: f32, b: f32| a + (b - a) * progress;
            let cover_size = lerp(26., 66.);
            let inset = lerp(7., 17.);
            let top = lerp(6., 16.);
            let cover = rect(inset, top, inset + cover_size, top + cover_size);
            let media = &track.media;
            let title = if media.title.is_empty() {
                &media.app
            } else {
                &media.title
            };
            let faded = |mut color: D2D1_COLOR_F, alpha: f32| {
                color.a *= alpha;
                color
            };
            let detail_opacity = ((progress - 0.45) / 0.55).clamp(0., 1.);
            let compact_opacity = (1. - progress * 1.6).clamp(0., 1.);
            unsafe {
                rt.Clear(Some(&palette.background));
                target
                    .bitmaps
                    .retain(|_, (pixels, _)| track.artwork.as_ref().is_some_and(|art| Arc::ptr_eq(art, pixels)));
                if let Some(pixels) = &track.artwork {
                    if !target.bitmaps.contains_key("cover") {
                        let bitmap = rt.CreateBitmap(
                            D2D_SIZE_U {
                                width: pixels.width,
                                height: pixels.height,
                            },
                            Some(pixels.bgra.as_ptr().cast()),
                            pixels.width * 4,
                            &D2D1_BITMAP_PROPERTIES {
                                pixelFormat: D2D1_PIXEL_FORMAT {
                                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                                },
                                dpiX: 96.,
                                dpiY: 96.,
                            },
                        )?;
                        target.bitmaps.insert("cover".into(), (pixels.clone(), bitmap));
                    }
                    let crop = pixels.width.min(pixels.height) as f32;
                    let x = (pixels.width as f32 - crop) / 2.;
                    let y = (pixels.height as f32 - crop) / 2.;
                    rt.DrawBitmap(
                        &target.bitmaps["cover"].1,
                        Some(&cover),
                        1.,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        Some(&rect(x, y, x + crop, y + crop)),
                    );
                } else {
                    brush.SetColor(&palette.tile);
                    rt.FillRoundedRectangle(&rounded(cover, 7.), brush);
                    brush.SetColor(&palette.text);
                    text(rt, brush, &formats.glyph, "\u{E8D6}", cover);
                }
                if compact_opacity > 0. {
                    brush.SetColor(&faded(palette.text, compact_opacity));
                    text(
                        rt,
                        brush,
                        &formats.row,
                        title,
                        rect(cover.right + 9., 0., width - 34., 38.),
                    );
                    brush.SetColor(&faded(palette.accent, compact_opacity));
                    let elapsed = std::time::UNIX_EPOCH.elapsed().unwrap_or_default().as_millis() % 2000;
                    for i in 0..3 {
                        let x = width - 25. + i as f32 * 5.;
                        let phase = elapsed as f32 * 0.009 + i as f32 * 1.5;
                        let h = if media.playing && super::client_animations_enabled() {
                            5. + 7. * phase.sin().abs()
                        } else {
                            7.
                        };
                        rt.FillRoundedRectangle(&rounded(rect(x, 19. - h / 2., x + 2.5, 19. + h / 2.), 1.25), brush);
                    }
                }
                if detail_opacity > 0. {
                    let left = cover.right + 14.;
                    brush.SetColor(&faded(palette.text, detail_opacity));
                    text(rt, brush, &formats.control_title, title, rect(left, 19., 304., 46.));
                    brush.SetColor(&faded(palette.subtext, detail_opacity));
                    let artist = if media.artist.is_empty() {
                        &media.app
                    } else {
                        &media.artist
                    };
                    text(rt, brush, &formats.control_detail, artist, rect(left, 45., 327., 69.));
                    text(rt, brush, &formats.glyph, "\u{E70E}", rect(310., 10., 342., 41.));

                    if let Some(timeline) = media.timeline {
                        let position = timeline.position(media.playing);
                        let ratio =
                            ((position - timeline.start) as f32 / (timeline.end - timeline.start) as f32).clamp(0., 1.);
                        let thumb = 24. + 306. * ratio;
                        brush.SetColor(&faded(palette.tile, detail_opacity));
                        rt.FillRoundedRectangle(&rounded(rect(24., 101., 330., 106.), 2.5), brush);
                        brush.SetColor(&faded(palette.accent, detail_opacity));
                        rt.FillRoundedRectangle(&rounded(rect(24., 101., thumb, 106.), 2.5), brush);
                        rt.FillEllipse(
                            &D2D1_ELLIPSE {
                                point: windows_numerics::Vector2 { X: thumb, Y: 103.5 },
                                radiusX: 4.5,
                                radiusY: 4.5,
                            },
                            brush,
                        );
                        brush.SetColor(&faded(palette.subtext, detail_opacity));
                        let clock = |ticks: i64| {
                            let seconds = ticks.max(0) / 10_000_000;
                            format!("{}:{:02}", seconds / 60, seconds % 60)
                        };
                        text(
                            rt,
                            brush,
                            &formats.small,
                            &clock(position - timeline.start),
                            rect(20., 109., 62., 129.),
                        );
                        text(
                            rt,
                            brush,
                            &formats.small,
                            &clock(timeline.end - timeline.start),
                            rect(289., 109., 334., 129.),
                        );
                    }
                    for (x, glyph, primary) in [
                        (117., glyph::PREVIOUS, false),
                        (177., if media.playing { glyph::PAUSE } else { glyph::PLAY }, true),
                        (237., glyph::NEXT, false),
                    ] {
                        let highlighted =
                            hover.is_some_and(|(hx, hy)| (hx - x).abs() < 24. && (130. ..170.).contains(&hy));
                        let bounds = rect(x - 22., 130., x + 22., 170.);
                        if primary || highlighted {
                            brush.SetColor(&faded(
                                if highlighted {
                                    palette.row_selected
                                } else {
                                    palette.tile
                                },
                                detail_opacity,
                            ));
                            rt.FillRoundedRectangle(&rounded(bounds, 18.), brush);
                        }
                        brush.SetColor(&faded(palette.text, detail_opacity));
                        text(rt, brush, &formats.glyph, glyph, bounds);
                    }
                }
            }
            Ok(())
        })
    }

    pub fn draw_flyout(&mut self, hwnd: HWND, frame: &FlyoutFrame) -> Result<()> {
        self.paint(hwnd, frame.dpi, frame.palette, |target, _| draw_flyout(target, frame))
    }

    pub fn draw_tip(&mut self, hwnd: HWND, dpi: u32, palette: &Palette, tip: &str) -> Result<()> {
        self.paint(hwnd, dpi, palette, |target, formats| {
            background(target, palette, 1.0);
            let size = unsafe { target.rt.GetSize() };
            unsafe { target.brush.SetColor(&palette.text) };
            text(
                &target.rt,
                &target.brush,
                &formats.tip,
                tip,
                rect(TIP_PAD.0, TIP_PAD.1, size.width - TIP_PAD.0, size.height),
            );
            Ok(())
        })
    }

    /// A tooltip's window size in DIPs, wrapping past `max` DIPs of text.
    pub fn measure_tip(&self, tip: &str, max: f32) -> Result<(f32, f32)> {
        let wide: Vec<u16> = tip.encode_utf16().collect();
        unsafe {
            let layout = self.dw.CreateTextLayout(&wide, &self.formats.tip, max, 1000.0)?;
            let mut metrics = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut metrics)?;
            Ok((
                metrics.widthIncludingTrailingWhitespace.ceil() + 2.0 * TIP_PAD.0,
                metrics.height.ceil() + 2.0 * TIP_PAD.1,
            ))
        }
    }

    /// Frame for a pointer-transparent, enlarged live DWM window thumbnail.
    pub fn draw_window_zoom(&mut self, hwnd: HWND, dpi: u32, palette: &Palette, title: &str) -> Result<()> {
        self.paint(hwnd, dpi, palette, |target, formats| {
            background(target, palette, 1.0);
            unsafe {
                target.brush.SetColor(&palette.text);
                text(
                    &target.rt,
                    &target.brush,
                    &formats.row,
                    title,
                    rect(12., 4., target.rt.GetSize().width - 12., 32.),
                );
            }
            Ok(())
        })
    }

    pub fn cached_popup_matches(&self, hwnd: HWND, dpi: u32) -> Result<bool> {
        self.layered
            .as_ref()
            .map_or(Ok(false), |surface| surface.cached_matches(hwnd, dpi))
    }
    pub fn present_cached_popup(&self, hwnd: HWND, dpi: u32, alpha: u8) -> Result<()> {
        self.layered
            .as_ref()
            .ok_or_else(|| windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))?
            .present_cached(hwnd, dpi, alpha)
    }

    /// Draws an app's windows as a list, highlighting the row under the pointer.
    #[expect(
        clippy::too_many_arguments,
        reason = "Draws borrowed window rows and preview geometry in a single frame"
    )]
    pub fn draw_windows(
        &mut self,
        hwnd: HWND,
        dpi: u32,
        palette: &Palette,
        rows: &[String],
        (monitors, monitor_height): (&[String], f32),
        hover: Option<(usize, bool)>,
        (card_width, preview_height): (f32, f32),
        previews: &[Option<isize>],
        (page, page_count): (usize, usize),
    ) -> Result<()> {
        self.paint(hwnd, dpi, palette, |target, formats| {
            background(target, palette, 1.0);
            let pad = logic::PREVIEW_PAD;
            let header = logic::PREVIEW_HEADER;
            let image_bottom = pad + header + preview_height;
            let bottom = image_bottom + monitor_height;
            let rt = &target.rt;
            let brush = &target.brush;
            unsafe {
                for (index, title) in rows.iter().enumerate() {
                    let left = pad + card_width * index as f32;
                    let right = left + card_width;
                    let hovered = hover.is_some_and(|(row, _)| row == index);
                    brush.SetColor(if hovered { &palette.row_selected } else { &palette.tile });
                    rt.FillRoundedRectangle(&rounded(rect(left + 2., pad, right - 2., bottom), 7.), brush);
                    brush.SetColor(&palette.text);
                    text(
                        rt,
                        brush,
                        &formats.row,
                        title,
                        rect(left + 9., pad, right - 31., pad + header),
                    );
                    let close = rect(right - 25., pad + 3., right - 5., pad + header - 3.);
                    if hover == Some((index, true)) {
                        brush.SetColor(&RED);
                        rt.FillRoundedRectangle(&rounded(close, 4.), brush);
                        brush.SetColor(&D2D1_COLOR_F {
                            r: 1.,
                            g: 1.,
                            b: 1.,
                            a: 1.,
                        });
                    }
                    text(rt, brush, &formats.glyph, "\u{E711}", close);
                    if preview_height >= 24. && previews.get(index).is_none_or(Option::is_none) {
                        brush.SetColor(&palette.subtext);
                        text(
                            rt,
                            brush,
                            &formats.small,
                            "Preview unavailable",
                            rect(left + 5., pad + header, right - 5., image_bottom),
                        );
                    }
                    if monitor_height > 0.
                        && let Some(label) = monitors.get(index)
                    {
                        brush.SetColor(&palette.subtext);
                        text(
                            rt,
                            brush,
                            &formats.small,
                            label,
                            rect(left + 5., image_bottom, right - 5., bottom),
                        );
                    }
                }
                if page_count > 1 {
                    let width = rt.GetSize().width;
                    let top = bottom + 3.;
                    let bottom = top + logic::PREVIEW_FOOTER;
                    brush.SetColor(if page > 0 { &palette.text } else { &palette.subtext });
                    text(rt, brush, &formats.small, "‹", rect(pad, top, pad + 32., bottom));
                    brush.SetColor(&palette.subtext);
                    text(
                        rt,
                        brush,
                        &formats.small,
                        &format!("{} / {page_count}", page + 1),
                        rect(width / 2. - 38., top, width / 2. + 38., bottom),
                    );
                    brush.SetColor(if page + 1 < page_count {
                        &palette.text
                    } else {
                        &palette.subtext
                    });
                    text(
                        rt,
                        brush,
                        &formats.small,
                        "›",
                        rect(width - pad - 32., top, width - pad, bottom),
                    );
                }
            }
            Ok(())
        })
    }

    /// Draws the desktop pill: a dot per desktop, the current one long, then its name.
    pub fn draw_desktops(
        &mut self,
        hwnd: HWND,
        dpi: u32,
        palette: &Palette,
        (current, count): (usize, usize),
        name: &str,
    ) -> Result<()> {
        self.paint(hwnd, dpi, palette, |target, formats| {
            background(target, palette, 1.0);
            let size = unsafe { target.rt.GetSize() };
            let middle = size.height / 2.0;
            let mut x = DESKTOP_PAD;
            for index in 0..count {
                let (width, color) = if index == current {
                    (DESKTOP_DOT_CURRENT, palette.text)
                } else {
                    (
                        DESKTOP_DOT,
                        D2D1_COLOR_F {
                            a: 0.35,
                            ..palette.text
                        },
                    )
                };
                unsafe {
                    target.brush.SetColor(&color);
                    target.rt.FillRoundedRectangle(
                        &rounded(
                            rect(x, middle - DESKTOP_DOT / 2.0, x + width, middle + DESKTOP_DOT / 2.0),
                            DESKTOP_DOT / 2.0,
                        ),
                        &target.brush,
                    );
                }
                x += width + DESKTOP_GAP;
            }
            unsafe { target.brush.SetColor(&palette.text) };
            let left = x - DESKTOP_GAP + DESKTOP_LABEL_GAP;
            text(
                &target.rt,
                &target.brush,
                &formats.tip,
                name,
                rect(left, TIP_PAD.1, size.width, size.height),
            );
            Ok(())
        })
    }

    /// The desktop pill's window size in DIPs.
    pub fn measure_desktops(&self, count: usize, name: &str) -> Result<(f32, f32)> {
        let (label, height) = self.measure_tip(name, 1000.0)?;
        let dots = count as f32 * (DESKTOP_DOT + DESKTOP_GAP) - DESKTOP_GAP + DESKTOP_DOT_CURRENT - DESKTOP_DOT;
        let label = label - 2.0 * TIP_PAD.0;
        Ok((2.0 * DESKTOP_PAD + dots + DESKTOP_LABEL_GAP + label, height))
    }

    /// Sizes the target to the window, then draws with `f` between BeginDraw and EndDraw.
    fn paint(
        &mut self,
        hwnd: HWND,
        dpi: u32,
        palette: &Palette,
        f: impl FnOnce(&mut Target, &Formats) -> Result<()>,
    ) -> Result<()> {
        unsafe {
            let mut client = RECT::default();
            GetClientRect(hwnd, &mut client)?;
            let size = D2D_SIZE_U {
                width: client.right.max(1) as u32,
                height: client.bottom.max(1) as u32,
            };
            if self.target.is_none() {
                let rt = self.factory.CreateHwndRenderTarget(
                    &D2D1_RENDER_TARGET_PROPERTIES {
                        r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                        pixelFormat: D2D1_PIXEL_FORMAT {
                            format: DXGI_FORMAT_B8G8R8A8_UNORM,
                            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                        },
                        ..Default::default()
                    },
                    &D2D1_HWND_RENDER_TARGET_PROPERTIES {
                        hwnd,
                        pixelSize: size,
                        presentOptions: D2D1_PRESENT_OPTIONS_IMMEDIATELY,
                    },
                )?;
                rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
                self.target = Some(Target {
                    brush: rt.CreateSolidColorBrush(&palette.text, None)?,
                    rt: rt.cast()?,
                    window: Some(rt),
                    bitmaps: HashMap::new(),
                    tray: HashMap::new(),
                    media_shapes: HashMap::new(),
                    launcher_shape: None,
                    dock_gradient: None,
                    size,
                });
            }
            let target = self.target.as_mut().unwrap();
            if target.size.width != size.width || target.size.height != size.height {
                target.window.as_ref().unwrap().Resize(&size)?;
                target.size = size;
            }
            target.rt.SetDpi(dpi as f32, dpi as f32);
            target.rt.BeginDraw();
            let drawn = f(target, &self.formats);
            let ended = target.rt.EndDraw(None, None);
            let result = drawn.and(ended);
            if result.as_ref().is_err_and(|e| e.code() == D2DERR_RECREATE_TARGET) {
                self.target = None;
                return Ok(());
            }
            result
        }
    }

    fn paint_layered(
        &mut self,
        hwnd: HWND,
        dpi: u32,
        palette: &Palette,
        f: impl FnOnce(&mut Target, &Formats) -> Result<()>,
    ) -> Result<()> {
        self.paint_surface(hwnd, dpi, palette, (layered::Contour::Rectangle, 255), f)
    }
    fn paint_surface(
        &mut self,
        hwnd: HWND,
        dpi: u32,
        palette: &Palette,
        presentation: (layered::Contour, u8),
        f: impl FnOnce(&mut Target, &Formats) -> Result<()>,
    ) -> Result<()> {
        self.paint_surface_with_alpha(hwnd, dpi, palette, presentation, 255, f)
    }
    fn paint_surface_with_alpha(
        &mut self,
        hwnd: HWND,
        dpi: u32,
        palette: &Palette,
        presentation: (layered::Contour, u8),
        alpha: u8,
        f: impl FnOnce(&mut Target, &Formats) -> Result<()>,
    ) -> Result<()> {
        if self.layered.is_none() {
            self.layered = Some(layered::Surface::new(&self.factory, palette)?);
        }
        let result = self
            .layered
            .as_mut()
            .unwrap()
            .paint_with_alpha(hwnd, dpi, &self.formats, presentation, alpha, f);
        if result.as_ref().is_err_and(|e| e.code() == D2DERR_RECREATE_TARGET) {
            self.layered = None;
        }
        result
    }
}

fn draw_island_content(target: &mut Target, formats: &Formats, frame: IslandFrame) -> Result<()> {
    let IslandFrame {
        dpi: _,
        palette,
        status,
        track,
        track_fade,
        previous_track,
        show_title,
        show_previous_title,
        equalizer,
        equalizer_color,
        reveal,
        mix,
        width,
        compact,
        time_width,
        date_width,
        hover,
        hover_level,
        press,
    } = frame;
    use super::topbar::model::Hit;

    let rt = target.rt.clone();
    let brush = target.brush.clone();
    let height = unsafe { rt.GetSize() }.height;
    let island = super::topbar::model::IslandLayout::new(width, compact, time_width);
    let mix = mix.clamp(0., 1.);
    island_background(target, palette);
    let color = |opacity: f32| D2D1_COLOR_F {
        a: opacity * reveal,
        ..palette.text
    };
    // The clock stays anchored at the physical center through every morph.
    unsafe {
        brush.SetColor(&color(1.));
    }
    text(
        &rt,
        &brush,
        &formats.island_clock,
        &status.time,
        rect(island.clock, 0., island.clock + time_width, height),
    );
    if mix < 1. {
        unsafe {
            brush.SetColor(&color(1. - mix));
        }
        text(
            &rt,
            &brush,
            &formats.small,
            &status.date,
            rect(
                14.,
                -mix * 3.,
                (14. + date_width).min(island.clock - 10.),
                height - mix * 3.,
            ),
        );
    }
    target.bitmaps.retain(|_, (pixels, _)| {
        [track, previous_track]
            .into_iter()
            .flatten()
            .any(|track| track.artwork.as_ref().is_some_and(|art| Arc::ptr_eq(pixels, art)))
    });
    if mix > 0. {
        let media = island.media(
            [track, previous_track]
                .into_iter()
                .flatten()
                .any(|track| track.artwork.is_some()),
        );
        for (track, weight, offset, title_visible) in [
            (previous_track, 1. - track_fade, -2. * track_fade, show_previous_title),
            (
                track,
                if previous_track.is_some() { track_fade } else { 1. },
                2. * (1. - track_fade),
                show_title,
            ),
        ] {
            let Some(track) = track else {
                continue;
            };
            if weight <= 0. {
                continue;
            }

            let track_alpha = mix * weight.clamp(0., 1.);
            let top = (height - 20.) / 2. + offset;
            let art = rect(8., top, 28., top + 20.);
            if let Some(pixels) = &track.artwork
                && media.artwork
            {
                let cached = target.bitmaps.values().find(|(cached, _)| Arc::ptr_eq(cached, pixels));
                let bitmap = if let Some((_, bitmap)) = cached {
                    bitmap
                } else {
                    let bitmap = unsafe {
                        rt.CreateBitmap(
                            D2D_SIZE_U {
                                width: pixels.width,
                                height: pixels.height,
                            },
                            Some(pixels.bgra.as_ptr().cast()),
                            pixels.width * 4,
                            &D2D1_BITMAP_PROPERTIES {
                                pixelFormat: D2D1_PIXEL_FORMAT {
                                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                                },
                                dpiX: 96.,
                                dpiY: 96.,
                            },
                        )
                    }?;
                    let cover_key = format!("{:p}", Arc::as_ptr(pixels));
                    &target.bitmaps.entry(cover_key).or_insert((pixels.clone(), bitmap)).1
                };
                unsafe {
                    rt.DrawBitmap(
                        bitmap,
                        Some(&art),
                        track_alpha,
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                        None,
                    );
                }
            }
            if title_visible && let Some((left, right)) = media.title {
                unsafe {
                    brush.SetColor(&color(track_alpha));
                }
                text(
                    &rt,
                    &brush,
                    &formats.control_detail,
                    if track.media.title.is_empty() {
                        &track.media.app
                    } else {
                        &track.media.title
                    },
                    rect(left, offset, right, height + offset),
                );
            }
        }
        let Some(track) = track.or(previous_track) else {
            return Ok(());
        };
        if let Some(left) = media.equalizer_left {
            unsafe {
                brush.SetColor(&D2D1_COLOR_F {
                    a: mix * reveal,
                    ..equalizer_color
                });
                for (i, bar_height) in equalizer.into_iter().enumerate() {
                    let x = left + EQUALIZER_BAR_STEP * i as f32;
                    let top = (height - bar_height) / 2.;
                    rt.FillRoundedRectangle(
                        &rounded(
                            rect(x, top, x + EQUALIZER_BAR_WIDTH, top + bar_height),
                            EQUALIZER_BAR_WIDTH / 2.,
                        ),
                        &brush,
                    );
                }
            }
        }
        for (hit, left) in [
            (Hit::Previous, island.controls[0]),
            (Hit::PlayPause, island.controls[1]),
            (Hit::Next, island.controls[2]),
        ] {
            if left + island.button > width {
                continue;
            }
            if hover == Some(hit) {
                highlight(target, palette, left, island.button, height, hover_level * mix);
            }
            let inset = if hover == Some(hit) { press * 2. } else { 0. };
            unsafe {
                brush.SetColor(&color(mix));
            }
            playback_icon(
                target,
                rect(left + inset, inset, left + island.button - inset, height - inset),
                hit,
                track.media.playing,
            )?;
        }
    }
    Ok(())
}

fn draw_topbar_overflow_content(target: &mut Target, frame: &TopbarFlyoutFrame) -> Result<()> {
    let TopbarFlyoutFrame {
        dpi,
        palette,
        shown,
        overflow,
        hover,
        presentation,
    } = frame;
    crate::theme::paint_background(
        &target.rt,
        &target.brush,
        palette,
        TOPBAR_FLYOUT_RADIUS * presentation.0,
    );
    unsafe {
        target
            .rt
            .SetTransform(&Matrix3x2::scale(presentation.0, presentation.0));
    }
    let result = (|| {
        for (i, &index) in overflow.iter().enumerate() {
            let (x, y) = logic::flyout_cell(i);
            if *hover == Some(i) {
                unsafe {
                    target.brush.SetColor(&palette.row_selected);
                    target
                        .rt
                        .FillRoundedRectangle(&rounded(rect(x + 2., y + 2., x + 38., y + 38.), 8.), &target.brush);
                }
            }
            if let Some(icon) = shown.get(index) {
                tray_icon(target, *dpi, icon, x + 20., y + 20.)?;
            }
        }
        Ok(())
    })();
    target.tray.retain(|key, _| shown.iter().any(|s| s.icon.key() == *key));
    unsafe {
        target.rt.SetTransform(&Matrix3x2::identity());
    }
    result
}

fn playback_icon(target: &mut Target, bounds: D2D_RECT_F, hit: super::topbar::model::Hit, playing: bool) -> Result<()> {
    use super::topbar::model::Hit;
    let cx = (bounds.left + bounds.right) / 2.;
    let cy = (bounds.top + bounds.bottom) / 2.;
    let half = 5.;
    unsafe {
        if hit == Hit::PlayPause && playing {
            for x in [cx - 4.5, cx + 1.5] {
                target
                    .rt
                    .FillRoundedRectangle(&rounded(rect(x, cy - half, x + 3., cy + half), 0.6), &target.brush);
            }
        } else {
            let direction = if hit == Hit::Previous { -1. } else { 1. };
            let shift = if hit == Hit::PlayPause { 0.5 } else { -direction };
            let key = match hit {
                Hit::Previous => 0,
                Hit::Next => 1,
                _ => 2,
            };
            if let std::collections::hash_map::Entry::Vacant(entry) = target.media_shapes.entry(key) {
                let factory = target.rt.GetFactory()?;
                let geometry = factory.CreatePathGeometry()?;
                let sink = geometry.Open()?;
                sink.BeginFigure(
                    windows_numerics::Vector2 {
                        X: shift - direction * 3.5,
                        Y: -half,
                    },
                    D2D1_FIGURE_BEGIN_FILLED,
                );
                sink.AddLine(windows_numerics::Vector2 {
                    X: shift + direction * 4.,
                    Y: 0.,
                });
                sink.AddLine(windows_numerics::Vector2 {
                    X: shift - direction * 3.5,
                    Y: half,
                });
                sink.EndFigure(D2D1_FIGURE_END_CLOSED);
                sink.Close()?;
                entry.insert(geometry);
            }
            let mut transform = Matrix3x2::identity();
            target.rt.GetTransform(&mut transform);
            target.rt.SetTransform(&(Matrix3x2::translation(cx, cy) * transform));
            target.rt.FillGeometry(&target.media_shapes[&key], &target.brush, None);
            target.rt.SetTransform(&transform);
            if hit != Hit::PlayPause {
                let x = cx + direction * 5.;
                target.rt.FillRoundedRectangle(
                    &rounded(rect(x - 0.8, cy - half, x + 0.8, cy + half), 0.5),
                    &target.brush,
                );
            }
        }
    }
    Ok(())
}

struct ControlFrame<'a> {
    palette: &'a Palette,
    elements: &'a [super::sound::Element],
    scroll: f32,
    max_scroll: f32,
    focus: Option<usize>,
    dragging: Option<(usize, u32)>,
    presentation: (f32, f32, f32),
}
fn draw_control_content(target: &mut Target, formats: &Formats, frame: ControlFrame) -> Result<()> {
    let ControlFrame {
        palette,
        elements,
        scroll,
        max_scroll,
        focus,
        dragging,
        presentation,
    } = frame;
    let (page_opacity, surface_scale, _) = presentation;
    use super::sound::Style;
    use crate::commands::glyph;

    let color = |hex: u32| D2D1_COLOR_F {
        r: ((hex >> 16) & 255) as f32 / 255.,
        g: ((hex >> 8) & 255) as f32 / 255.,
        b: (hex & 255) as f32 / 255.,
        a: 1.,
    };
    unsafe {
        let rt = &target.rt;
        let brush = &target.brush;
        let size = rt.GetSize();
        rt.SetTransform(&Matrix3x2::identity());
        crate::theme::paint_background(rt, brush, palette, 18. * surface_scale);
        rt.SetTransform(&Matrix3x2 {
            M11: surface_scale,
            M22: surface_scale,
            ..Matrix3x2::identity()
        });
        let size = D2D_SIZE_F {
            width: size.width / surface_scale,
            height: size.height / surface_scale,
        };
        if page_opacity < 1. {
            rt.PushLayer(
                &D2D1_LAYER_PARAMETERS {
                    contentBounds: rect(0., 0., size.width, size.height),
                    opacity: page_opacity,
                    maskTransform: Matrix3x2::identity(),
                    ..Default::default()
                },
                None,
            );
        }
        let rgb = |color: &D2D1_COLOR_F| {
            ((color.r * 255.) as u32) << 16 | ((color.g * 255.) as u32) << 8 | (color.b * 255.) as u32
        };
        let surface = |strength: f32| {
            let base = &palette.background_solid;
            let ink = &palette.text;
            rgb(&D2D1_COLOR_F {
                r: base.r + (ink.r - base.r) * strength,
                g: base.g + (ink.g - base.g) * strength,
                b: base.b + (ink.b - base.b) * strength,
                a: 1.,
            })
        };
        let card = surface(0.13);
        let row = surface(0.17);
        let hover = surface(0.24);
        let muted = surface(0.68);
        let main = rgb(&palette.text);
        let fill = |r, radius, hex| {
            brush.SetColor(&color(hex));
            rt.FillRoundedRectangle(&rounded(r, radius), brush);
        };
        let text = |label: &str, format: &IDWriteTextFormat, r, hex| {
            if !label.is_empty() {
                brush.SetColor(&color(hex));
                let units: Vec<u16> = label.encode_utf16().collect();
                rt.DrawText(
                    &units,
                    format,
                    &r,
                    brush,
                    D2D1_DRAW_TEXT_OPTIONS_CLIP,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }
        };
        for (i, item) in elements.iter().enumerate() {
            let b = item.bounds;
            let (x, y, w, h) = (b.x, b.y - scroll, b.w, b.h);
            if y >= size.height || y + h <= 0. {
                continue;
            }
            let selected = focus == Some(i);
            let bounds = rect(x, y, x + w, y + h);
            match item.style {
                Style::Header => {
                    let start = if item.icon.is_empty() { x } else { x + 36. };
                    text(item.icon, &formats.glyph, rect(x, y, x + 28., y + h), main);
                    text(
                        &item.title,
                        &formats.heading_leading,
                        rect(
                            start,
                            y,
                            start + w - (start - x),
                            y + if item.detail.is_empty() { h } else { 26. },
                        ),
                        main,
                    );
                    text(
                        &item.detail,
                        &formats.control_detail,
                        rect(start, y + 26., x + w, y + h),
                        muted,
                    );
                }
                Style::Toggle(on) => {
                    fill(
                        rect(x + 2., y + 2., x + w - 2., y + h - 2.),
                        (h - 4.) / 2.,
                        if on { 0x087ff2 } else { surface(0.3) },
                    );
                    let left = if on { x + w - h + 2. } else { x + 4. };
                    fill(rect(left, y + 4., left + h - 8., y + h - 4.), (h - 8.) / 2., 0xf4f5f7);
                }
                Style::CurrentRow => {
                    fill(bounds, 12., card);
                    text(
                        item.icon,
                        &formats.glyph,
                        rect(x + 12., y + 8., x + 36., y + h - 8.),
                        main,
                    );
                    text(
                        &item.title,
                        &formats.control_title,
                        rect(x + 46., y + 5., x + w - 12., y + 28.),
                        main,
                    );
                    text(
                        &item.detail,
                        &formats.control_detail,
                        rect(x + 46., y + 28., x + w - 12., y + h - 4.),
                        muted,
                    );
                }
                Style::CompactCard(active) => {
                    fill(bounds, 14., if selected || active { hover } else { row });
                    text(item.icon, &formats.glyph, rect(x + 10., y + 9., x + 32., y + 34.), main);
                    text(
                        &item.title,
                        &formats.control_detail,
                        rect(x + 40., y + 8., x + w - 12., y + 34.),
                        main,
                    );
                    text(
                        &item.detail,
                        &formats.control_detail,
                        rect(x + 12., y + 30., x + w - 12., y + h - 4.),
                        muted,
                    );
                    if active {
                        text(
                            glyph::CHECK,
                            &formats.chevron,
                            rect(x + w - 28., y + 36., x + w - 10., y + h - 8.),
                            main,
                        );
                    }
                }
                Style::Footer => {
                    if selected {
                        fill(bounds, 10., row);
                    }
                    let icon_w = if item.icon.is_empty() { 0. } else { 26. };
                    text(item.icon, &formats.glyph, rect(x + 4., y, x + 30., y + h), muted);
                    text(
                        &item.title,
                        &formats.control_detail,
                        rect(x + 8. + icon_w, y, x + w - 4., y + h),
                        muted,
                    );
                }
                Style::CalendarDay {
                    today,
                    selected: day_selected,
                    in_month,
                } => {
                    let inset = (w - h.min(36.)) / 2.;
                    let circle = rect(x + inset, y + 2., x + w - inset, y + h - 2.);
                    if today {
                        fill(circle, 18., 0x087ff2);
                    } else if day_selected {
                        fill(circle, 18., row);
                    } else if selected && item.is_interactive() {
                        fill(circle, 18., hover);
                    }
                    if day_selected && !today {
                        brush.SetColor(&color(muted));
                        rt.DrawRoundedRectangle(&rounded(circle, 18.), brush, 1., None);
                    }
                    text(
                        &item.title,
                        &formats.small,
                        bounds,
                        if today {
                            0xffffff
                        } else if in_month {
                            main
                        } else {
                            muted
                        },
                    );
                }
                Style::Card => {
                    fill(bounds, 18., card);
                    text(
                        &item.title,
                        &formats.control_title,
                        rect(x + 15., y + 9., x + w - 12., y + 34.),
                        main,
                    );
                }
                Style::Tile(active) => {
                    fill(bounds, 10., if selected { hover } else { card });
                    let cy = y + h / 2.;
                    text(
                        item.icon,
                        &formats.glyph,
                        rect(x + 10., cy - 10., x + 30., cy + 10.),
                        if active { rgb(&palette.accent) } else { main },
                    );
                    text(
                        &item.title,
                        &formats.control_detail,
                        rect(x + 40., cy - 20., x + w - 20., cy + 2.),
                        main,
                    );
                    text(
                        &item.detail,
                        &formats.control_detail,
                        rect(x + 40., cy + 1., x + w - 20., cy + 22.),
                        muted,
                    );
                    text(
                        "\u{E76C}",
                        &formats.chevron,
                        rect(x + w - 18., y, x + w - 5., y + h),
                        muted,
                    );
                }
                Style::Row(active) => {
                    fill(bounds, 12., if selected || active { hover } else { row });
                    let start = if item.icon.is_empty() { x + 12. } else { x + 39. };
                    text(
                        item.icon,
                        &formats.glyph,
                        if item.title.is_empty() {
                            bounds
                        } else {
                            rect(x + 7., y, x + 36., y + h)
                        },
                        main,
                    );
                    if item.detail.is_empty() || item.detail == "✓" || item.detail.ends_with('›') {
                        let tail = match item.detail.as_str() {
                            "" => 9.,
                            "✓" | "›" => 36.,
                            _ => 62.,
                        };
                        text(
                            &item.title,
                            &formats.control_detail,
                            rect(start, y, x + w - tail, y + h),
                            main,
                        );
                        text(
                            &item.detail,
                            &formats.small,
                            rect(x + w - tail + 2., y, x + w - 9., y + h),
                            if active { 0x58aaff } else { muted },
                        );
                    } else {
                        text(
                            &item.title,
                            &formats.control_title,
                            rect(start, y + 1., x + w - 10., y + h * 0.55),
                            main,
                        );
                        text(
                            &item.detail,
                            &formats.control_detail,
                            rect(start, y + h * 0.48, x + w - 10., y + h),
                            muted,
                        );
                    }
                }
                Style::Slider(value) => {
                    let value = dragging
                        .filter(|(index, _)| *index == i)
                        .map_or(value, |(_, value)| value);
                    let start = x + 36.;
                    let end = x + w - 8.;
                    let thumb = start + (end - start) * value as f32 / 100.;
                    let cy = y + h / 2.;
                    fill(rect(start, cy - 2., end, cy + 2.), 2., surface(0.32));
                    if thumb > start {
                        fill(rect(start, cy - 2., thumb, cy + 2.), 2., main);
                    }
                    fill(rect(thumb - 7., cy - 7., thumb + 7., cy + 7.), 7., main);
                    text(item.icon, &formats.glyph, rect(x, y, x + 24., y + h), main);
                }
                Style::MediaTitle => {
                    let subtitle = !item.detail.is_empty();
                    let format = if subtitle {
                        &formats.control_detail
                    } else {
                        &formats.control_title
                    };
                    text(&item.title, format, bounds, if subtitle { muted } else { main });
                }
                Style::MediaButton(primary) => {
                    if selected || primary {
                        fill(bounds, h / 2., if selected { hover } else { row });
                    }
                    text(item.icon, &formats.glyph, bounds, main);
                }
                Style::Timeline(timeline, playing) => {
                    let duration = timeline.end - timeline.start;
                    let offset = dragging.filter(|(index, _)| *index == i).map_or_else(
                        || timeline.position(playing) - timeline.start,
                        |(_, percent)| (duration as f64 * f64::from(percent) / 100.) as i64,
                    );
                    let thumb = x + w * (offset as f64 / duration as f64) as f32;
                    let bar = y + 11.;
                    fill(rect(x, bar - 2., x + w, bar + 2.), 2., 0x606066);
                    if thumb > x {
                        fill(rect(x, bar - 2., thumb, bar + 2.), 2., 0xefeff0);
                    }
                    if item.is_interactive() {
                        fill(rect(thumb - 4., bar - 4., thumb + 4., bar + 4.), 4., 0xffffff);
                    }
                    let time = |ticks: i64| {
                        let seconds = ticks / 10_000_000;
                        if seconds >= 3600 {
                            format!("{}:{:02}:{:02}", seconds / 3600, seconds / 60 % 60, seconds % 60)
                        } else {
                            format!("{}:{:02}", seconds / 60, seconds % 60)
                        }
                    };
                    text(
                        &time(offset),
                        &formats.control_detail,
                        rect(x, y + 23., x + 65., y + h),
                        muted,
                    );
                    text(
                        &time(duration),
                        &formats.small,
                        rect(x + w - 54., y + 23., x + w, y + h),
                        muted,
                    );
                }
                Style::Section => {
                    text(&item.title, &formats.control_title, bounds, main);
                }
                Style::Text | Style::CalendarSelection => {
                    if selected {
                        fill(bounds, 12., hover);
                    }
                    let split = if item.detail.is_empty() { w } else { w * 0.68 };
                    text(
                        &item.title,
                        &formats.control_detail,
                        rect(x + 4., y, x + split, y + h),
                        main,
                    );
                    text(
                        &item.detail,
                        &formats.control_detail,
                        rect(x + split, y, x + w, y + h),
                        muted,
                    );
                }
            }
            if selected {
                brush.SetColor(&color(0x75b7ff));
                rt.DrawRoundedRectangle(&rounded(bounds, 12.), brush, 1., None);
            }
        }
        if max_scroll > 0. {
            let track = (size.height - 32.).max(0.);
            let thumb = (track * size.height / (size.height + max_scroll)).clamp(18_f32.min(track), track);
            let top = 16. + (track - thumb) * (scroll / max_scroll).clamp(0., 1.);
            fill(rect(size.width - 7., top, size.width - 4., top + thumb), 1.5, muted);
        }
        if page_opacity < 1. {
            rt.PopLayer();
        }
    }
    Ok(())
}

/// A tooltip's text inset, in DIPs.
const TIP_PAD: (f32, f32) = (8.0, 5.0);
/// The desktop pill's dots and spacing, in DIPs.
const DESKTOP_PAD: f32 = 10.0;
const DESKTOP_DOT: f32 = 6.0;
const DESKTOP_DOT_CURRENT: f32 = 16.0;
const DESKTOP_GAP: f32 = 5.0;
const DESKTOP_LABEL_GAP: f32 = 9.0;

fn rect(left: f32, top: f32, right: f32, bottom: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left,
        top,
        right,
        bottom,
    }
}

fn island_radius(height: f32) -> f32 {
    7_f32.min(height * 0.3)
}
fn island_background(target: &Target, palette: &Palette) {
    let size = unsafe { target.rt.GetSize() };
    let (width, height) = (size.width, size.height);
    unsafe {
        target.rt.Clear(Some(&D2D1_COLOR_F {
            r: 0.,
            g: 0.,
            b: 0.,
            a: 0.,
        }));
        // Alpha-zero ULW pixels pass input through. Keep the complete island
        // hit surface while its configured tint remains visually transparent.
        target.brush.SetColor(&D2D1_COLOR_F {
            a: palette.background.a.max(1. / 255.),
            ..palette.background
        });
        target.rt.FillRoundedRectangle(
            &rounded(rect(0., 0., width, height), island_radius(height).min(width / 2.)),
            &target.brush,
        );
    }
}

/// Alpha-zero layered pixels pass input through. Only actual control cells get
/// one alpha step, invisible in practice but retaining the full mouse target.
fn hit_backdrop(target: &Target, area: D2D_RECT_F) {
    unsafe {
        target.brush.SetColor(&D2D1_COLOR_F {
            r: 0.,
            g: 0.,
            b: 0.,
            a: 1. / 255.,
        });
        target.rt.FillRectangle(&area, &target.brush);
    }
}

fn highlight(target: &Target, palette: &Palette, x: f32, width: f32, height: f32, alpha: f32) {
    unsafe {
        target.brush.SetColor(&D2D1_COLOR_F {
            a: 0.13 * alpha,
            ..palette.text
        });
        target.rt.FillRoundedRectangle(
            &rounded(rect(x + 2., 2., x + width - 2., height - 2.), 6.),
            &target.brush,
        );
    }
}
fn rounded(rect: D2D_RECT_F, radius: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT {
        rect,
        radiusX: radius,
        radiusY: radius,
    }
}

/// The rounded highlight behind a hovered or active slot or part.
fn tile(target: &Target, frame: &Frame, left: f32, right: f32, color: &D2D1_COLOR_F) {
    unsafe {
        target.brush.SetColor(color);
        target.rt.FillRoundedRectangle(
            &rounded(
                frame.snap_rect(
                    left,
                    TILE_TOP * frame.layout_scale,
                    right,
                    TILE_BOTTOM * frame.layout_scale,
                ),
                TILE_RADIUS * frame.layout_scale,
            ),
            &target.brush,
        );
    }
}

/// DWM rounds the corners and draws the backdrop; this only tints it.
fn background(target: &Target, palette: &Palette, layout_scale: f32) {
    crate::theme::paint_background(&target.rt, &target.brush, palette, 8.0 * layout_scale);
}

fn paint_dock_gradient(target: &mut Target, palette: &Palette, radius: f32, gradient: Gradient) -> Result<()> {
    let rt = &target.rt;
    let size = unsafe { rt.GetSize() };
    let (end_x, end_y) = match gradient.direction {
        Direction::LeftToRight => (size.width, 0.),
        Direction::TopToBottom => (0., size.height),
        Direction::Diagonal => {
            // The two axes contribute equally even on a very wide dock.
            let factor = 2. * size.width * size.height / (size.width.powi(2) + size.height.powi(2));
            (size.height * factor, size.width * factor)
        }
    };
    let end = windows_numerics::Vector2 { X: end_x, Y: end_y };
    unsafe {
        if target
            .dock_gradient
            .as_ref()
            .is_none_or(|(cached, _)| *cached != gradient)
        {
            let color = |[r, g, b]: [u8; 3]| D2D1_COLOR_F {
                r: f32::from(r) / 255.,
                g: f32::from(g) / 255.,
                b: f32::from(b) / 255.,
                a: (100 - gradient.transparency.min(100)) as f32 / 100.,
            };
            let stops = rt.CreateGradientStopCollection(
                &[
                    D2D1_GRADIENT_STOP {
                        position: 0.,
                        color: color(gradient.start),
                    },
                    D2D1_GRADIENT_STOP {
                        position: 1.,
                        color: color(gradient.end),
                    },
                ],
                D2D1_GAMMA_2_2,
                D2D1_EXTEND_MODE_CLAMP,
            )?;
            let brush = rt.CreateLinearGradientBrush(
                &D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                    startPoint: windows_numerics::Vector2::default(),
                    endPoint: end,
                },
                None,
                &stops,
            )?;
            target.dock_gradient = Some((gradient, brush));
        }
        let (_, brush) = target.dock_gradient.as_ref().unwrap();
        brush.SetEndPoint(end);
        rt.Clear(None);
        rt.FillRectangle(&rect(0., 0., size.width, size.height), brush);
    }
    crate::theme::paint_background_border(rt, &target.brush, palette, radius);
    Ok(())
}

/// Draws a notification icon centered on (`x`, `y`). Its pixels are made
/// for this DPI, so they map one to one onto the screen.
fn tray_icon(target: &mut Target, dpi: u32, shown: &Shown, x: f32, y: f32) -> Result<()> {
    let Some(pixels) = &shown.pixels else { return Ok(()) };
    let key = shown.icon.key();
    let scale = dpi as f32 / 96.0;
    let stale = target.tray.get(&key).is_none_or(|(old, _)| !Arc::ptr_eq(old, pixels));
    unsafe {
        if stale {
            let bitmap = target.rt.CreateBitmap(
                D2D_SIZE_U {
                    width: pixels.width,
                    height: pixels.height,
                },
                Some(pixels.bgra.as_ptr().cast()),
                pixels.width * 4,
                &D2D1_BITMAP_PROPERTIES {
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                    },
                    dpiX: 96.0,
                    dpiY: 96.0,
                },
            )?;
            target.tray.insert(key, (pixels.clone(), bitmap));
        }
        let (width, height) = (pixels.width as f32 / scale, pixels.height as f32 / scale);
        let left = ((x - width / 2.0) * scale).round() / scale;
        let top = ((y - height / 2.0) * scale).round() / scale;
        target.rt.DrawBitmap(
            &target.tray[&key].1,
            Some(&rect(left, top, left + width, top + height)),
            1.0,
            D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
            None,
        );
    }
    Ok(())
}

fn draw_flyout(target: &mut Target, frame: &FlyoutFrame) -> Result<()> {
    background(target, frame.palette, 1.0);
    target
        .tray
        .retain(|key, _| frame.tray.iter().any(|shown| shown.icon.key() == *key));
    let scale = frame.dpi as f32 / 96.0;
    let snap = |dip: f32| (dip * scale).round() / scale;
    let cell = logic::FLYOUT_CELL;
    for (index, shown) in frame.tray.iter().enumerate() {
        let (left, top) = logic::flyout_cell(index);
        if frame.hover == Some(index) {
            unsafe {
                target.brush.SetColor(&frame.palette.row_selected);
                target.rt.FillRoundedRectangle(
                    &rounded(
                        rect(
                            snap(left + 2.0),
                            snap(top + 2.0),
                            snap(left + cell - 2.0),
                            snap(top + cell - 2.0),
                        ),
                        TILE_RADIUS,
                    ),
                    &target.brush,
                );
            }
        }
        tray_icon(target, frame.dpi, shown, left + cell / 2.0, top + cell / 2.0)?;
    }
    Ok(())
}

/// A one-pixel vertical rule at `x`, between the apps and the status.
fn separator(target: &Target, frame: &Frame, x: f32) {
    let x = frame.snap(x);
    unsafe {
        target.brush.SetColor(&frame.palette.divider);
        target.rt.FillRectangle(
            &frame.snap_rect(
                x,
                17.0 * frame.layout_scale,
                x + frame.hairline(),
                39.0 * frame.layout_scale,
            ),
            &target.brush,
        );
    }
}

fn draw_command_mark(target: &mut Target, left: f32, top: f32, size: f32) -> Result<()> {
    unsafe {
        if target.launcher_shape.is_none() {
            let geometry = target.rt.GetFactory()?.CreatePathGeometry()?;
            let sink = geometry.Open()?;
            let point = |x, y| windows_numerics::Vector2 { X: x, Y: y };
            let arc = |x, y| D2D1_ARC_SEGMENT {
                point: point(x, y),
                size: D2D_SIZE_F { width: 3., height: 3. },
                rotationAngle: 0.,
                sweepDirection: D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE,
                arcSize: D2D1_ARC_SIZE_LARGE,
            };
            // Four open-sided loops joined by the central square, on a 24 DIP canvas.
            sink.BeginFigure(point(19., 8.), D2D1_FIGURE_BEGIN_HOLLOW);
            sink.AddArc(&arc(16., 5.));
            sink.AddLine(point(16., 19.));
            sink.AddArc(&arc(19., 16.));
            sink.AddLine(point(5., 16.));
            sink.AddArc(&arc(8., 19.));
            sink.AddLine(point(8., 5.));
            sink.AddArc(&arc(5., 8.));
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            sink.Close()?;
            target.launcher_shape = Some(geometry);
        }
        let mut transform = Matrix3x2::identity();
        target.rt.GetTransform(&mut transform);
        let scale = size / 24.;
        target
            .rt
            .SetTransform(&(Matrix3x2::scale(scale, scale) * Matrix3x2::translation(left, top) * transform));
        target
            .rt
            .DrawGeometry(target.launcher_shape.as_ref().unwrap(), &target.brush, 2., None);
        target.rt.SetTransform(&transform);
    }
    Ok(())
}

fn draw_content(target: &mut Target, formats: &Formats, frame: &Frame) -> Result<()> {
    unsafe {
        if frame.animation_scale < 1.0 {
            target.rt.SetTransform(&Matrix3x2 {
                M11: frame.animation_scale,
                M22: frame.animation_scale,
                ..Matrix3x2::identity()
            });
        }
        let result = draw(target, formats, frame).and_then(|()| draw_status(target, formats, frame));
        if frame.animation_scale < 1.0 {
            target.rt.SetTransform(&Matrix3x2::identity());
        }
        result
    }
}

unsafe fn draw(target: &mut Target, formats: &Formats, frame: &Frame) -> Result<()> {
    unsafe {
        let Frame {
            palette: p,
            items,
            icons,
            hover,
            app_icon_size,
            scroll,
            ..
        } = frame;
        let running = frame.count();
        let foreground = GetForegroundWindow().0 as isize;
        target.bitmaps.retain(|key, _| icons.contains_key(key));
        let lifted = frame.drag.as_ref().map(|drag| drag.from);
        // Paint the moving icon last so it stays above its neighbors.
        for index in (0..=running).filter(|&index| Some(index) != lifted).chain(lifted) {
            let x = logic::left_scaled(index, frame.layout_scale);
            let slot = logic::slot(frame.layout_scale);
            let item = index.checked_sub(1).and_then(|i| items.get(i + scroll));
            let active = item.is_some_and(|item| item.windows.iter().any(|window| window.hwnd == foreground));
            if active {
                tile(
                    target,
                    frame,
                    x + 2.0 * frame.layout_scale,
                    x + slot - 2.0 * frame.layout_scale,
                    &p.tile,
                );
            }
            if *hover == Some(index) {
                tile(
                    target,
                    frame,
                    x + 2.0 * frame.layout_scale,
                    x + slot - 2.0 * frame.layout_scale,
                    &p.row_selected,
                );
            }
            let progress = frame.hover_levels.get(index).copied().unwrap_or(0.0);
            let moving = frame.drag.as_ref().filter(|drag| drag.from == index);
            let lift = if moving.is_some() { 6.0 } else { 2.0 } * progress * progress * (3.0 - 2.0 * progress);
            let icon_size = *app_icon_size * if moving.is_some() { 1.12 } else { 1.0 };
            let icon_top = ICON_CENTER_Y * frame.layout_scale - icon_size / 2.0 - lift * frame.layout_scale;
            let icon_x = moving.map_or(x, |drag| {
                (x + drag.offset).clamp(
                    logic::left_scaled(1, frame.layout_scale),
                    logic::left_scaled(running, frame.layout_scale),
                )
            });
            let icon_rect = frame.snap_rect(
                icon_x + (slot - icon_size) / 2.0,
                icon_top,
                icon_x + (slot + icon_size) / 2.0,
                icon_top + icon_size,
            );
            let Some(item) = item else {
                // The Command mark is vector geometry, independent of installed icon fonts.
                target.brush.SetColor(&p.text);
                let start_size = icon_size * 0.9;
                let half = start_size / 2.;
                let left = (icon_rect.left + icon_rect.right) / 2. - half;
                let top = (icon_rect.top + icon_rect.bottom) / 2. - half;
                draw_command_mark(target, left, top, start_size)?;
                continue;
            };
            let fallback = item
                .app
                .path
                .as_ref()
                .map(|path| format!("exe:{}", path.to_lowercase()));
            let icon = icons
                .get_key_value(&item.app.id)
                .or_else(|| fallback.as_ref().and_then(|key| icons.get_key_value(key)));
            if let Some((key, pixels)) = icon {
                let stale = target.bitmaps.get(key).is_none_or(|(old, _)| !Arc::ptr_eq(old, pixels));
                if stale {
                    let bitmap = target.rt.CreateBitmap(
                        D2D_SIZE_U {
                            width: pixels.width,
                            height: pixels.height,
                        },
                        Some(pixels.bgra.as_ptr().cast()),
                        pixels.width * 4,
                        &D2D1_BITMAP_PROPERTIES {
                            pixelFormat: D2D1_PIXEL_FORMAT {
                                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                            },
                            dpiX: 96.0,
                            dpiY: 96.0,
                        },
                    )?;
                    target.bitmaps.insert(key.clone(), (pixels.clone(), bitmap));
                }
                target.rt.DrawBitmap(
                    &target.bitmaps[key].1,
                    Some(&icon_rect),
                    1.0,
                    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                    None,
                );
            } else {
                target.brush.SetColor(&p.tile);
                target.rt.FillRoundedRectangle(&rounded(icon_rect, 6.0), &target.brush);
                target.brush.SetColor(&p.text);
                let initial = item.app.name.chars().next().unwrap_or('?').to_uppercase().to_string();
                text(&target.rt, &target.brush, &formats.letter, &initial, icon_rect);
            }
            if !item.windows.is_empty() {
                // A short dash for running apps, longer and in accent for the foreground one.
                let half = if active { 6.0 } else { 1.5 };
                let center = x + slot / 2.0;
                target.brush.SetColor(if active { &p.accent } else { &p.subtext });
                target.rt.FillRoundedRectangle(
                    &rounded(
                        frame.snap_rect(
                            center - half * frame.layout_scale,
                            INDICATOR_TOP * frame.layout_scale,
                            center + half * frame.layout_scale,
                            (INDICATOR_TOP + 3.0) * frame.layout_scale,
                        ),
                        1.5 * frame.layout_scale,
                    ),
                    &target.brush,
                );
            }
        }
        if let Some(drag) = &frame.drag
            && let Some(to) = drag.to
        {
            let edge = to + usize::from(to > drag.from);
            let scale = frame.layout_scale;
            let x = logic::left_scaled(edge, scale);
            target.brush.SetColor(&p.accent);
            target.rt.FillRoundedRectangle(
                &rounded(frame.snap_rect(x - scale, 12. * scale, x + scale, 40. * scale), scale),
                &target.brush,
            );
        }
        Ok(())
    }
}

fn text(
    rt: &ID2D1RenderTarget,
    brush: &ID2D1SolidColorBrush,
    format: &IDWriteTextFormat,
    text: &str,
    area: D2D_RECT_F,
) {
    let text: Vec<u16> = text.encode_utf16().collect();
    unsafe {
        rt.DrawText(
            &text,
            format,
            &area,
            brush,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            DWRITE_MEASURING_MODE_NATURAL,
        );
    }
}

/// Segoe Fluent Icons battery in tenths, from Battery0 (U+EBA0) or BatteryCharging0 (U+EBAB).
fn battery_glyph(percent: u32, charging: bool) -> String {
    let first = if charging { 0xEBAB } else { 0xEBA0 };
    char::from_u32(first + (percent.min(100) + 5) / 10).map_or_else(String::new, String::from)
}

/// A separator, then the volume, battery and clock, each with a second line.
fn draw_status(target: &mut Target, formats: &Formats, frame: &Frame) -> Result<()> {
    let Frame {
        palette: p,
        status,
        hover_part,
        parts,
        ..
    } = frame;
    let running = frame.count();
    target
        .tray
        .retain(|key, _| frame.tray.iter().any(|shown| shown.icon.key() == *key));
    separator(
        target,
        frame,
        logic::width_scaled(running, frame.layout_scale) - logic::pad(frame.layout_scale)
            + logic::separator_width(frame.layout_scale) / 2.0,
    );
    for &part in parts.iter() {
        let Some(left) = logic::part_left_scaled(part, running, parts, frame.layout_scale) else {
            continue;
        };
        let right = left + part.width_scaled(frame.layout_scale);
        if *hover_part == Some(part)
            || part == Part::Chevron && frame.flyout_open
            || part == Part::Volume && frame.sound_open
        {
            tile(
                target,
                frame,
                left + 2.0 * frame.layout_scale,
                right - 2.0 * frame.layout_scale,
                &p.row_selected,
            );
        }
        match part {
            Part::Chevron => {
                unsafe { target.brush.SetColor(&p.text) };
                // Up opens the flyout and down closes it, as on the taskbar.
                let glyph = if frame.flyout_open { "\u{E70D}" } else { "\u{E70E}" };
                let area = rect(
                    left,
                    TILE_TOP * frame.layout_scale,
                    right,
                    TILE_BOTTOM * frame.layout_scale,
                );
                text(&target.rt, &target.brush, &formats.chevron, glyph, area);
                continue;
            }
            Part::Tray(index) => {
                if let Some(shown) = frame.inline.get(index).and_then(|&i| frame.tray.get(i)) {
                    let middle = (TILE_TOP + TILE_BOTTOM) / 2.0 * frame.layout_scale;
                    tray_icon(
                        target,
                        frame.dpi,
                        shown,
                        left + part.width_scaled(frame.layout_scale) / 2.0,
                        middle,
                    )?;
                }
                continue;
            }
            _ => {}
        }
        let (rt, brush) = (&target.rt, &target.brush);
        let upper = rect(left, UPPER.0 * frame.layout_scale, right, UPPER.1 * frame.layout_scale);
        let lower = rect(left, LOWER.0 * frame.layout_scale, right, LOWER.1 * frame.layout_scale);
        unsafe {
            match part {
                Part::Volume => {
                    brush.SetColor(&p.text);
                    let tile = rect(
                        left,
                        TILE_TOP * frame.layout_scale,
                        right,
                        TILE_BOTTOM * frame.layout_scale,
                    );
                    text(rt, brush, &formats.glyph, "\u{E9E9}", tile);
                }
                Part::Battery => {
                    let Some(level) = status.battery else { continue };
                    let low = level.percent <= 20 && !level.charging;
                    brush.SetColor(if low { &RED } else { &p.text });
                    text(
                        rt,
                        brush,
                        &formats.glyph,
                        &battery_glyph(level.percent, level.charging),
                        upper,
                    );
                    brush.SetColor(&p.subtext);
                    text(rt, brush, &formats.small, &format!("{}%", level.percent), lower);
                }
                Part::Clock => {
                    brush.SetColor(&p.text);
                    text(rt, brush, &formats.time, &status.time, upper);
                    brush.SetColor(&p.subtext);
                    text(rt, brush, &formats.small, &status.date, lower);
                }
                Part::Chevron | Part::Tray(_) => {}
            }
        }
    }
    Ok(())
}

const RED: D2D1_COLOR_F = D2D1_COLOR_F {
    r: 0.91,
    g: 0.30,
    b: 0.27,
    a: 1.0,
};
