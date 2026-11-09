//! Small Direct2D renderer, separate from the launcher's renderer.

use super::logic::Part;
use super::status::Status;
use super::tray::Shown;
use super::{Item, logic};
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
use windows::core::{PCWSTR, Result, w};
use windows_numerics::Matrix3x2;

/// Vertical layout, in DIPs, shared by the app slots and the status parts.
/// Icons sit a little above center to leave room for the running indicator.
use super::logic::{ICON_CENTER_Y, INDICATOR_TOP, TILE_BOTTOM, TILE_RADIUS, TILE_TOP};
const UPPER: (f32, f32) = (8.0, 29.0);
const LOWER: (f32, f32) = (28.0, 48.0);

struct Target {
    rt: ID2D1HwndRenderTarget,
    brush: ID2D1SolidColorBrush,
    bitmaps: HashMap<String, (Arc<IconPixels>, ID2D1Bitmap)>,
    /// Notification-area icons, by their tray identity.
    tray: HashMap<Key, (Arc<IconPixels>, ID2D1Bitmap)>,
    size: D2D_SIZE_U,
}

#[derive(Clone)]
struct Formats {
    /// A fallback tile's initial.
    letter: IDWriteTextFormat,
    time: IDWriteTextFormat,
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
}

pub struct Frame<'a> {
    pub animation_scale: f32,
    pub dpi: u32,
    pub palette: &'a Palette,
    pub items: &'a [Item],
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
}

/// The notification-area flyout's grid.
pub struct FlyoutFrame<'a> {
    pub dpi: u32,
    pub palette: &'a Palette,
    pub tray: &'a [Shown],
    pub hover: Option<usize>,
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
        Ok(Formats {
            letter: format(display, DWRITE_FONT_WEIGHT_SEMI_BOLD, 15.0)?,
            time: format(text, DWRITE_FONT_WEIGHT_SEMI_BOLD, 14.0)?,
            small: format(text, DWRITE_FONT_WEIGHT_NORMAL, 12.5)?,
            glyph: format(icons, DWRITE_FONT_WEIGHT_NORMAL, 17.0)?,
            chevron: format(icons, DWRITE_FONT_WEIGHT_NORMAL, 12.0)?,
            tip,
            row,
            control_title,
            control_detail,
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
        }
    }

    pub fn draw(&mut self, hwnd: HWND, frame: &Frame) -> Result<()> {
        self.update_formats(frame.layout_scale)?;
        self.paint(hwnd, frame.dpi, frame.palette, |target, formats| {
            background(target, frame.palette, frame.layout_scale);
            draw_content(target, formats, frame)
        })
    }

    pub fn draw_on(&mut self, rt: &ID2D1HwndRenderTarget, frame: &Frame) -> Result<()> {
        self.update_formats(frame.layout_scale)?;
        if self.shared_target.as_ref().is_none_or(|t| t.rt != *rt) {
            self.shared_target = Some(Target {
                rt: rt.clone(),
                brush: unsafe { rt.CreateSolidColorBrush(&frame.palette.text, None)? },
                bitmaps: HashMap::new(),
                tray: HashMap::new(),
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
        page_opacity: f32,
    ) -> Result<()> {
        use super::sound::Style;
        self.paint(hwnd, dpi, palette, |target, formats| {
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
                crate::theme::paint_background(rt, brush, palette, 30.);
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
                            fill(bounds, 18., if selected { hover } else { card });
                            let cy = y + h / 2.;
                            fill(
                                rect(x + 15., cy - 20., x + 55., cy + 20.),
                                20.,
                                if active { 0x087ff2 } else { 0x606065 },
                            );
                            text(
                                item.icon,
                                &formats.glyph,
                                rect(x + 15., cy - 20., x + 55., cy + 20.),
                                main,
                            );
                            text(
                                &item.title,
                                &formats.control_title,
                                rect(x + 67., cy - 22., x + w - 8., cy + 3.),
                                main,
                            );
                            text(
                                &item.detail,
                                &formats.control_detail,
                                rect(x + 67., cy + 1., x + w - 8., cy + 25.),
                                muted,
                            );
                        }
                        Style::Row(active) => {
                            fill(bounds, 12., if selected || active { hover } else { row });
                            let start = if item.icon.is_empty() { x + 12. } else { x + 39. };
                            text(item.icon, &formats.glyph, rect(x + 7., y, x + 36., y + h), main);
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
                            let thumb = x + 26. + (w - 39.) * value as f32 / 100.;
                            fill(bounds, 16., surface(0.26));
                            fill(rect(x, y, (thumb + 14.).min(x + w), y + h), 16., 0xefeff0);
                            fill(rect(thumb - 12., y + 3., thumb + 12., y + h - 3.), 13., 0xffffff);
                            text(item.icon, &formats.glyph, rect(x + 3., y, x + 35., y + h), 0x616167);
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
                        Style::Text => {
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

    /// Draws an app's windows as a list, highlighting the row under the pointer.
    pub fn draw_windows(
        &mut self,
        hwnd: HWND,
        dpi: u32,
        palette: &Palette,
        rows: &[String],
        hover: Option<(usize, bool)>,
        (card_width, preview_height): (f32, f32),
        previews: &[Option<isize>],
        (page, page_count): (usize, usize),
    ) -> Result<()> {
        self.paint(hwnd, dpi, palette, |target, formats| {
            background(target, palette, 1.0);
            let pad = logic::PREVIEW_PAD;
            let header = logic::PREVIEW_HEADER;
            let bottom = pad + header + preview_height;
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
                            rect(left + 5., pad + header, right - 5., bottom),
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
                    rt,
                    bitmaps: HashMap::new(),
                    tray: HashMap::new(),
                    size,
                });
            }
            let target = self.target.as_mut().unwrap();
            if target.size.width != size.width || target.size.height != size.height {
                target.rt.Resize(&size)?;
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
        for index in 0..=running {
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
            let lift = 2.0 * progress * progress * (3.0 - 2.0 * progress);
            let icon_size = *app_icon_size;
            let icon_top = ICON_CENTER_Y * frame.layout_scale - icon_size / 2.0 - lift * frame.layout_scale;
            let icon_rect = frame.snap_rect(
                x + (slot - icon_size) / 2.0,
                icon_top,
                x + (slot + icon_size) / 2.0,
                icon_top + icon_size,
            );
            let Some(item) = item else {
                // Draw the four panes directly so Start does not depend on an icon font.
                target.brush.SetColor(&p.text);
                let start_size = icon_size * 0.85;
                let gap = 2. * frame.layout_scale;
                let pane = (start_size - gap) / 2.;
                let step = pane + gap;
                let half = start_size / 2.;
                let left = (icon_rect.left + icon_rect.right) / 2. - half;
                let top = (icon_rect.top + icon_rect.bottom) / 2. - half;
                for y in [top, top + step] {
                    for x in [left, left + step] {
                        let bounds = frame.snap_rect(x, y, x + pane, y + pane);
                        target.rt.FillRectangle(&bounds, &target.brush);
                    }
                }
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
        Ok(())
    }
}

fn text(
    rt: &ID2D1HwndRenderTarget,
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

/// Segoe Fluent Icons speaker: muted, silent, or one to three waves.

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
        if *hover_part == Some(part) || part == Part::Chevron && frame.flyout_open {
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
