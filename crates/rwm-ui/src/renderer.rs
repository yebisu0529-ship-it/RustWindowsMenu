use std::collections::HashMap;
use std::sync::Arc;

use rwm_core::model::{BgraIcon, EntryAction, MenuEntry, MenuItem};
use windows::core::{Error as WinError, Result as WinResult, HSTRING};
use windows::Win32::Foundation::{COLORREF, D2DERR_RECREATE_TARGET, HWND, RECT, SIZE};
use windows::Win32::Globalization::GetUserDefaultLocaleName;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
    D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Bitmap, ID2D1Factory, ID2D1HwndRenderTarget, ID2D1SolidColorBrush,
    D2D1_ANTIALIAS_MODE_ALIASED, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, D2D1_BITMAP_PROPERTIES,
    D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT, D2D1_FACTORY_OPTIONS,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT,
    D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_PRESENT_OPTIONS_IMMEDIATELY,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE,
    D2D1_ROUNDED_RECT,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
    DWRITE_FONT_WEIGHT_BOLD, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT, DWRITE_TEXT_ALIGNMENT_CENTER,
    DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_TEXT_METRICS,
    DWRITE_TRIMMING, DWRITE_TRIMMING_GRANULARITY_CHARACTER, DWRITE_WORD_WRAPPING_NO_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateDIBSection,
    CreateFontIndirectW, CreatePen, CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint,
    FillRect, GdiAlphaBlend, GetStockObject, GetTextExtentPoint32W, IntersectClipRect,
    InvalidateRect, RestoreDC, RoundRect, SaveDC, SelectObject, SetBkMode, SetTextColor,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION,
    CLEARTYPE_QUALITY, DEFAULT_CHARSET, DIB_RGB_COLORS, DRAW_TEXT_FORMAT, DT_CENTER,
    DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, FONT_WEIGHT,
    FW_BOLD, FW_NORMAL, HBITMAP, HDC, HFONT, LOGFONTW, NULL_BRUSH, PAINTSTRUCT, PS_SOLID, SRCCOPY,
    TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

use crate::theme::{Color, MenuMetrics, ThemeColors};

// Layout constants in DIPs. Multiply by `MenuMetrics::dpi_scale`.
const PILL_INSET: f32 = 4.0;
const SHORTCUT_GAP: f32 = 24.0;
const CHEVRON_WIDTH: f32 = 14.0;
const MIN_WIDTH: f32 = 180.0;
const MAX_WIDTH: f32 = 560.0;
const SCROLL_BUTTON_ROW_FRACTION: f32 = 0.75;
const BADGE_PADDING: f32 = 6.0;
const SUBMENU_OVERLAP: f32 = 4.0;

const FOOTER_FONT_RATIO: f32 = 0.9;
const GLYPH_FONT_RATIO: f32 = 1.15;
const DISABLED_ICON_OPACITY: f32 = 0.4;
const MAX_TARGET_RECREATES: u8 = 3;

const CHEVRON_GLYPH: &str = "\u{203A}";
const CHECK_GLYPH: &str = "\u{2713}";
const SCROLL_UP_GLYPH: &str = "\u{25B4}";
const SCROLL_DOWN_GLYPH: &str = "\u{25BE}";

/// Measured size of one entry, the only text-dependent input to [`compute_layout`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RowSpec {
    Separator,
    Item {
        label_width: f32,
        shortcut_width: f32,
    },
}

/// One row in content coordinates. `label_right` is where this row's label must stop so that it
/// never runs into its own shortcut hint.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RowBox {
    pub top: f32,
    pub height: f32,
    pub is_separator: bool,
    pub label_right: f32,
}

/// Pixel geometry of one popup level. `rows[i]` lays out `entries[i]`.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuLayout {
    pub width: f32,
    pub height: f32,
    pub rows: Vec<RowBox>,
    /// Padding plus every row, before any viewport clipping.
    pub content_height: f32,
    pub footer_height: f32,
    /// Zero unless the content is taller than the work area.
    pub scroll_button_height: f32,
    pub pill_left: f32,
    pub pill_right: f32,
    pub icon_left: f32,
    pub label_left: f32,
    pub shortcut_right: f32,
    pub chevron_left: f32,
    pub chevron_right: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitTarget {
    Row(usize),
    ScrollUp,
    ScrollDown,
    Footer,
    Nothing,
}

pub fn compute_layout(
    specs: &[RowSpec],
    metrics: &MenuMetrics,
    footer_visible: bool,
    max_height: f32,
) -> MenuLayout {
    let scale = metrics.dpi_scale;
    let pill_left = (PILL_INSET * scale).round();
    let icon_left = pill_left + metrics.padding_x;
    let label_left = icon_left + metrics.icon_size + metrics.padding_x;
    let chevron_width = (CHEVRON_WIDTH * scale).round();
    let shortcut_gap = (SHORTCUT_GAP * scale).round();

    let mut top = metrics.padding_y;
    let mut widest_label = 0.0f32;
    let mut widest_shortcut = 0.0f32;
    let mut stacked = Vec::with_capacity(specs.len());
    for spec in specs {
        let (height, shortcut_width) = match *spec {
            RowSpec::Separator => (metrics.separator_height, None),
            RowSpec::Item {
                label_width,
                shortcut_width,
            } => {
                widest_label = widest_label.max(label_width);
                widest_shortcut = widest_shortcut.max(shortcut_width);
                (metrics.row_height, Some(shortcut_width))
            }
        };
        stacked.push((top, height, shortcut_width));
        top += height;
    }
    let content_height = top + metrics.padding_y;

    let shortcut_column = if widest_shortcut > 0.0 {
        shortcut_gap + widest_shortcut.ceil()
    } else {
        0.0
    };
    let trailing = chevron_width + metrics.padding_x + pill_left;
    let natural_width = label_left + widest_label.ceil() + shortcut_column + trailing;
    let width = natural_width
        .clamp((MIN_WIDTH * scale).round(), (MAX_WIDTH * scale).round())
        .ceil();

    let pill_right = width - pill_left;
    let chevron_right = pill_right - metrics.padding_x;
    let chevron_left = chevron_right - chevron_width;
    let shortcut_right = chevron_left;
    // Once the width is clamped, an oversized hint is clipped rather than squeezing its label away.
    let narrowest_label_right = label_left + (shortcut_right - label_left) / 2.0;

    let rows = stacked
        .into_iter()
        .map(|(top, height, shortcut_width)| RowBox {
            top,
            height,
            is_separator: shortcut_width.is_none(),
            label_right: match shortcut_width {
                Some(w) if w > 0.0 => {
                    (shortcut_right - w.ceil() - shortcut_gap).max(narrowest_label_right)
                }
                _ => chevron_left,
            },
        })
        .collect();

    let footer_height = if footer_visible {
        metrics.footer_height
    } else {
        0.0
    };
    let natural_height = content_height + footer_height;
    let (height, scroll_button_height) = if natural_height > max_height {
        let button = (metrics.row_height * SCROLL_BUTTON_ROW_FRACTION).round();
        let smallest_usable = footer_height + 2.0 * button + metrics.row_height;
        (max_height.floor().max(smallest_usable), button)
    } else {
        (natural_height, 0.0)
    };

    MenuLayout {
        width,
        height,
        rows,
        content_height,
        footer_height,
        scroll_button_height,
        pill_left,
        pill_right,
        icon_left,
        label_left,
        shortcut_right,
        chevron_left,
        chevron_right,
    }
}

impl MenuLayout {
    pub fn is_scrollable(&self) -> bool {
        self.scroll_button_height > 0.0
    }

    pub fn viewport_top(&self) -> f32 {
        self.scroll_button_height
    }

    pub fn viewport_bottom(&self) -> f32 {
        self.height - self.footer_height - self.scroll_button_height
    }

    pub fn footer_top(&self) -> f32 {
        self.height - self.footer_height
    }

    pub fn max_scroll(&self) -> f32 {
        (self.content_height - (self.viewport_bottom() - self.viewport_top())).max(0.0)
    }

    pub fn clamp_scroll(&self, scroll: f32) -> f32 {
        scroll.clamp(0.0, self.max_scroll())
    }

    /// Top of `row` in window coordinates for the given scroll offset.
    pub fn row_top(&self, row: usize, scroll: f32) -> f32 {
        self.rows[row].top - scroll + self.viewport_top()
    }

    pub fn pixel_size(&self) -> (i32, i32) {
        (self.width.ceil() as i32, self.height.ceil() as i32)
    }

    pub fn hit_test(&self, x: f32, y: f32, scroll: f32) -> HitTarget {
        if x < 0.0 || y < 0.0 || x >= self.width || y >= self.height {
            return HitTarget::Nothing;
        }
        if y >= self.footer_top() {
            return HitTarget::Footer;
        }
        if self.is_scrollable() {
            if y < self.viewport_top() {
                return HitTarget::ScrollUp;
            }
            if y >= self.viewport_bottom() {
                return HitTarget::ScrollDown;
            }
        }
        let content_y = y - self.viewport_top() + scroll;
        let index = self
            .rows
            .partition_point(|row| row.top + row.height <= content_y);
        match self.rows.get(index) {
            Some(row) if row.top <= content_y => HitTarget::Row(index),
            _ => HitTarget::Nothing,
        }
    }

    /// Smallest scroll change that shows all of `row`.
    pub fn scroll_to_reveal(&self, row: usize, scroll: f32) -> f32 {
        let Some(target) = self.rows.get(row) else {
            return self.clamp_scroll(scroll);
        };
        if !self.is_scrollable() {
            return 0.0;
        }
        let view = self.viewport_bottom() - self.viewport_top();
        let revealed = if target.top < scroll {
            if row == 0 {
                0.0
            } else {
                target.top
            }
        } else if target.top + target.height > scroll + view {
            if row + 1 == self.rows.len() {
                self.max_scroll()
            } else {
                target.top + target.height - view
            }
        } else {
            scroll
        };
        self.clamp_scroll(revealed)
    }
}

/// Integer screen rectangle with exclusive `right` and `bottom`, like Win32 `RECT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PixelRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl PixelRect {
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    pub const fn from_origin_size(origin: (i32, i32), size: (i32, i32)) -> Self {
        Self::new(origin.0, origin.1, origin.0 + size.0, origin.1 + size.1)
    }

    pub const fn width(&self) -> i32 {
        self.right - self.left
    }

    pub const fn height(&self) -> i32 {
        self.bottom - self.top
    }
}

impl From<RECT> for PixelRect {
    fn from(rect: RECT) -> Self {
        Self::new(rect.left, rect.top, rect.right, rect.bottom)
    }
}

fn clamp_axis(start: i32, len: i32, low: i32, high: i32) -> i32 {
    if len >= high - low {
        low
    } else {
        start.clamp(low, high - len)
    }
}

/// Places a root popup at `anchor`, flipping left and/or up when it would cross the work area's
/// right or bottom edge, then clamping so the popup never leaves the work area.
pub fn clamp_popup_origin(
    anchor: (i32, i32),
    size: (i32, i32),
    work_area: PixelRect,
) -> (i32, i32) {
    let (width, height) = size;
    let x = if anchor.0.saturating_add(width) > work_area.right {
        anchor.0 - width
    } else {
        anchor.0
    };
    let y = if anchor.1.saturating_add(height) > work_area.bottom {
        anchor.1 - height
    } else {
        anchor.1
    };
    (
        clamp_axis(x, width, work_area.left, work_area.right),
        clamp_axis(y, height, work_area.top, work_area.bottom),
    )
}

/// Places a cascading submenu beside the screen rectangle of the row that owns it: to the right
/// unless that crosses the work area, otherwise to the left, with its first row level with the owner.
pub fn clamp_submenu_origin(
    owner_row: PixelRect,
    size: (i32, i32),
    work_area: PixelRect,
    overlap: i32,
    padding_y: i32,
) -> (i32, i32) {
    let (width, height) = size;
    let right_side = owner_row.right - overlap;
    let x = if right_side.saturating_add(width) > work_area.right {
        owner_row.left + overlap - width
    } else {
        right_side
    };
    let level = owner_row.top - padding_y;
    let y = if level.saturating_add(height) > work_area.bottom {
        owner_row.bottom + padding_y - height
    } else {
        level
    };
    (
        clamp_axis(x, width, work_area.left, work_area.right),
        clamp_axis(y, height, work_area.top, work_area.bottom),
    )
}

/// Pixel overlap between a submenu and its parent popup.
pub fn submenu_overlap(metrics: &MenuMetrics) -> i32 {
    (SUBMENU_OVERLAP * metrics.dpi_scale).round() as i32
}

/// New origin for a popup that changed size while open. The edge nearest the anchor stays put, so
/// a growing menu extends away from the cursor and only shifts as far as the work area forces it.
pub fn reflow_origin(
    current: PixelRect,
    anchor: (i32, i32),
    new_size: (i32, i32),
    work_area: PixelRect,
) -> (i32, i32) {
    reflow_origin_anchored(current, anchor, new_size, work_area, None)
}

/// Like [`reflow_origin`], but when `pinned_row` supplies `(desired_screen_top, new_row_top)`,
/// keeps that row at the same screen Y coordinate unless the work area forces a clamp.
pub fn reflow_origin_anchored(
    current: PixelRect,
    anchor: (i32, i32),
    new_size: (i32, i32),
    work_area: PixelRect,
    pinned_row: Option<(f32, f32)>,
) -> (i32, i32) {
    let (width, height) = new_size;
    let x = if current.left < anchor.0 {
        current.right - width
    } else {
        current.left
    };
    let y = match pinned_row {
        Some((desired_screen_top, new_row_top)) => (desired_screen_top - new_row_top).round() as i32,
        None if current.top < anchor.1 => current.bottom - height,
        None => current.top,
    };
    (
        clamp_axis(x, width, work_area.left, work_area.right),
        clamp_axis(y, height, work_area.top, work_area.bottom),
    )
}

/// What the renderer needs to draw one popup level.
pub struct MenuFrame<'a> {
    pub entries: &'a [MenuEntry],
    pub layout: &'a MenuLayout,
    pub highlighted: Option<usize>,
    pub scroll: f32,
    /// Radius of the 1px outline; zero when DWM leaves the window corners square.
    pub outline_radius: f32,
    pub footer_status: Option<&'a str>,
    pub footer_badge: Option<&'a str>,
}

/// Creates a Direct2D factory and a DirectWrite factory, then releases them.
pub fn check_direct2d_available() -> Result<(), String> {
    create_factories()
        .map(|_| ())
        .map_err(|error| format!("Direct2D/DirectWrite unavailable: {error}"))
}

fn create_factories() -> WinResult<(ID2D1Factory, IDWriteFactory)> {
    unsafe {
        let d2d: ID2D1Factory = D2D1CreateFactory(
            D2D1_FACTORY_TYPE_SINGLE_THREADED,
            Some(&D2D1_FACTORY_OPTIONS::default()),
        )?;
        let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        Ok((d2d, dwrite))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextStyle {
    Label,
    Bold,
    Shortcut,
    Footer,
    Glyph,
}

/// Draws popup levels with Direct2D, or with double-buffered GDI when Direct2D is unavailable.
pub struct MenuRenderer {
    theme: ThemeColors,
    metrics: MenuMetrics,
    font_family: String,
    backend: Backend,
    print_gdi: Option<GdiBackend>,
}

enum Backend {
    Direct2D(D2dBackend),
    Gdi(GdiBackend),
}

#[cfg(test)]
thread_local! {
    /// Lets smoke tests drive the GDI fallback on machines where Direct2D works.
    pub(crate) static FORCE_GDI: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn gdi_forced() -> bool {
    FORCE_GDI.with(std::cell::Cell::get)
}

#[cfg(not(test))]
fn gdi_forced() -> bool {
    false
}

impl MenuRenderer {
    pub fn new(hwnd: HWND, theme: &ThemeColors, metrics: &MenuMetrics, font_family: &str) -> Self {
        let direct2d = if gdi_forced() {
            None
        } else {
            D2dBackend::new(hwnd, metrics, font_family)
                .inspect_err(|error| {
                    tracing::warn!(%error, "Direct2D unavailable; using the GDI fallback renderer");
                })
                .ok()
        };
        let backend = match direct2d {
            Some(d2d) => Backend::Direct2D(d2d),
            None => Backend::Gdi(GdiBackend::new(metrics, font_family)),
        };
        Self {
            theme: theme.clone(),
            metrics: *metrics,
            font_family: font_family.to_owned(),
            backend,
            print_gdi: None,
        }
    }

    pub fn is_direct2d(&self) -> bool {
        matches!(self.backend, Backend::Direct2D(_))
    }

    pub fn measure_rows(&self, entries: &[MenuEntry]) -> Vec<RowSpec> {
        entries
            .iter()
            .map(|entry| match entry {
                MenuEntry::Separator => RowSpec::Separator,
                MenuEntry::Item(item) => RowSpec::Item {
                    label_width: self.measure(&item.label, label_style(item)),
                    shortcut_width: item
                        .shortcut_hint
                        .as_deref()
                        .map_or(0.0, |hint| self.measure(hint, TextStyle::Shortcut)),
                },
            })
            .collect()
    }

    fn measure(&self, text: &str, style: TextStyle) -> f32 {
        match &self.backend {
            Backend::Direct2D(d2d) => {
                measure_with_dwrite(&d2d.dwrite, d2d.formats.get(style), text)
            }
            Backend::Gdi(gdi) => measure_with_gdi(gdi.measure_dc, gdi.fonts.get(style), text),
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        let Backend::Direct2D(d2d) = &mut self.backend else {
            return;
        };
        d2d.size = D2D_SIZE_U {
            width: width.max(1),
            height: height.max(1),
        };
        if let Some(target) = &d2d.target {
            if unsafe { target.rt.Resize(&d2d.size) }.is_err() {
                d2d.target = None;
            }
        }
    }

    /// Handles `WM_PAINT`: validates the window and draws the whole frame.
    pub fn paint(&mut self, hwnd: HWND, frame: &MenuFrame) {
        let mut ps = PAINTSTRUCT::default();
        let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
        let result = match &mut self.backend {
            Backend::Direct2D(d2d) => d2d.render(frame, &self.theme, &self.metrics),
            Backend::Gdi(gdi) => {
                gdi.render(hdc, frame, &self.theme, &self.metrics);
                Ok(())
            }
        };
        unsafe {
            let _ = EndPaint(hwnd, &ps);
        }
        if let Err(error) = result {
            self.recover(hwnd, &error);
        }
    }

    /// Renders `frame` into a caller-supplied device context (`WM_PRINTCLIENT` / `PrintWindow`).
    pub fn paint_to_dc(&mut self, hdc: HDC, frame: &MenuFrame) {
        if hdc.is_invalid() {
            return;
        }
        match &mut self.backend {
            Backend::Gdi(gdi) => gdi.render(hdc, frame, &self.theme, &self.metrics),
            Backend::Direct2D(_) => {
                let metrics = self.metrics;
                let family = self.font_family.as_str();
                let gdi = self
                    .print_gdi
                    .get_or_insert_with(|| GdiBackend::new(&metrics, family));
                gdi.render(hdc, frame, &self.theme, &self.metrics);
            }
        }
    }

    fn recover(&mut self, hwnd: HWND, error: &WinError) {
        let Backend::Direct2D(d2d) = &mut self.backend else {
            return;
        };
        d2d.target = None;
        d2d.failures += 1;
        if error.code() != D2DERR_RECREATE_TARGET || d2d.failures > MAX_TARGET_RECREATES {
            tracing::warn!(%error, "Direct2D rendering failed; switching to the GDI fallback renderer");
            self.backend = Backend::Gdi(GdiBackend::new(&self.metrics, &self.font_family));
        }
        unsafe {
            let _ = InvalidateRect(hwnd, None, false);
        }
    }
}

fn label_style(item: &MenuItem) -> TextStyle {
    if item.is_default {
        TextStyle::Bold
    } else {
        TextStyle::Label
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct RectF {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

impl RectF {
    const fn new(left: f32, top: f32, right: f32, bottom: f32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    fn height(&self) -> f32 {
        self.bottom - self.top
    }

    fn inset(self, by: f32) -> Self {
        Self::new(
            self.left + by,
            self.top + by,
            self.right - by,
            self.bottom - by,
        )
    }

    fn to_d2d(self) -> D2D_RECT_F {
        D2D_RECT_F {
            left: self.left,
            top: self.top,
            right: self.right,
            bottom: self.bottom,
        }
    }

    fn to_gdi(self) -> RECT {
        RECT {
            left: self.left.round() as i32,
            top: self.top.round() as i32,
            right: self.right.round() as i32,
            bottom: self.bottom.round() as i32,
        }
    }
}

/// Drawing primitives shared by both backends, so the menu's look is defined once in [`draw_frame`].
trait Canvas {
    fn clear(&mut self, color: Color);
    fn fill_rect(&mut self, rect: RectF, color: Color);
    fn fill_rounded(&mut self, rect: RectF, radius: f32, color: Color);
    /// 1px outline whose outer edge lies on `rect`.
    fn stroke_rounded(&mut self, rect: RectF, radius: f32, color: Color);
    fn text(&mut self, text: &str, style: TextStyle, rect: RectF, color: Color);
    fn icon(&mut self, icon: &Arc<BgraIcon>, rect: RectF, opacity: f32);
    fn measure(&mut self, text: &str, style: TextStyle) -> f32;
    fn push_clip(&mut self, rect: RectF);
    fn pop_clip(&mut self);
}

fn draw_frame(
    canvas: &mut impl Canvas,
    frame: &MenuFrame,
    theme: &ThemeColors,
    metrics: &MenuMetrics,
) {
    let layout = frame.layout;
    canvas.clear(theme.bg);
    if layout.footer_height > 0.0 {
        draw_footer(canvas, frame, theme, metrics);
    }

    let viewport = RectF::new(
        0.0,
        layout.viewport_top(),
        layout.width,
        layout.viewport_bottom(),
    );
    canvas.push_clip(viewport);
    for (index, (entry, row)) in frame.entries.iter().zip(&layout.rows).enumerate() {
        let top = layout.row_top(index, frame.scroll);
        let bounds = RectF::new(0.0, top, layout.width, top + row.height);
        if bounds.bottom <= viewport.top || bounds.top >= viewport.bottom {
            continue;
        }
        match entry {
            MenuEntry::Separator => {
                let thickness = metrics.dpi_scale.floor().max(1.0);
                let y = (bounds.top + (row.height - thickness) / 2.0).round();
                let line = RectF::new(layout.pill_left, y, layout.pill_right, y + thickness);
                canvas.fill_rect(line, theme.separator);
            }
            MenuEntry::Item(item) => {
                let highlighted = frame.highlighted == Some(index);
                draw_item(
                    canvas,
                    item,
                    row,
                    bounds,
                    highlighted,
                    layout,
                    theme,
                    metrics,
                );
            }
        }
    }
    canvas.pop_clip();

    if layout.is_scrollable() {
        let up = RectF::new(0.0, 0.0, layout.width, layout.viewport_top());
        let down = RectF::new(
            0.0,
            layout.viewport_bottom(),
            layout.width,
            layout.footer_top(),
        );
        let enabled = |active: bool| {
            if active {
                theme.chevron
            } else {
                theme.text_disabled
            }
        };
        canvas.text(
            SCROLL_UP_GLYPH,
            TextStyle::Glyph,
            up,
            enabled(frame.scroll > 0.0),
        );
        canvas.text(
            SCROLL_DOWN_GLYPH,
            TextStyle::Glyph,
            down,
            enabled(frame.scroll < layout.max_scroll()),
        );
    }

    let outline = RectF::new(0.0, 0.0, layout.width, layout.height);
    canvas.stroke_rounded(outline, frame.outline_radius, theme.border);
}

#[allow(clippy::too_many_arguments)]
fn draw_item(
    canvas: &mut impl Canvas,
    item: &MenuItem,
    row: &RowBox,
    bounds: RectF,
    highlighted: bool,
    layout: &MenuLayout,
    theme: &ThemeColors,
    metrics: &MenuMetrics,
) {
    if highlighted {
        let inset = metrics.dpi_scale.round().max(1.0);
        let pill = RectF::new(
            layout.pill_left,
            bounds.top + inset,
            layout.pill_right,
            bounds.bottom - inset,
        );
        canvas.fill_rounded(pill, metrics.corner_radius, theme.hover_bg);
        canvas.stroke_rounded(pill, metrics.corner_radius, theme.hover_border);
    }

    let (text, secondary, chevron) = if item.enabled {
        (theme.text, theme.shortcut_text, theme.chevron)
    } else {
        (
            theme.text_disabled,
            theme.text_disabled,
            theme.text_disabled,
        )
    };

    let icon_top = (bounds.top + (row.height - metrics.icon_size) / 2.0).round();
    let gutter = RectF::new(
        layout.icon_left,
        icon_top,
        layout.icon_left + metrics.icon_size,
        icon_top + metrics.icon_size,
    );
    if item.checked {
        let column = RectF::new(gutter.left, bounds.top, gutter.right, bounds.bottom);
        canvas.text(CHECK_GLYPH, TextStyle::Glyph, column, text);
    } else if let Some(icon) = &item.icon {
        let opacity = if item.enabled {
            1.0
        } else {
            DISABLED_ICON_OPACITY
        };
        canvas.icon(icon, gutter, opacity);
    }

    let label = RectF::new(
        layout.label_left,
        bounds.top,
        row.label_right,
        bounds.bottom,
    );
    canvas.text(&item.label, label_style(item), label, text);
    if let Some(hint) = &item.shortcut_hint {
        let column = RectF::new(
            row.label_right,
            bounds.top,
            layout.shortcut_right,
            bounds.bottom,
        );
        canvas.text(hint, TextStyle::Shortcut, column, secondary);
    }
    if matches!(item.action, EntryAction::Submenu(_)) {
        let column = RectF::new(
            layout.chevron_left,
            bounds.top,
            layout.chevron_right,
            bounds.bottom,
        );
        canvas.text(CHEVRON_GLYPH, TextStyle::Glyph, column, chevron);
    }
}

fn draw_footer(
    canvas: &mut impl Canvas,
    frame: &MenuFrame,
    theme: &ThemeColors,
    metrics: &MenuMetrics,
) {
    let layout = frame.layout;
    let top = layout.footer_top();
    canvas.fill_rect(
        RectF::new(0.0, top, layout.width, layout.height),
        theme.footer_bg,
    );
    canvas.fill_rect(
        RectF::new(0.0, top, layout.width, top + 1.0),
        theme.separator,
    );

    let scale = metrics.dpi_scale;
    let mut status_right = layout.pill_right;
    if let Some(badge) = frame.footer_badge {
        let padding = (BADGE_PADDING * scale).round();
        let inset = (3.0 * scale).round();
        let text_width = canvas.measure(badge, TextStyle::Footer).ceil();
        let pill = RectF::new(
            layout.pill_right - text_width - 2.0 * padding,
            top + 1.0 + inset,
            layout.pill_right,
            layout.height - inset,
        );
        canvas.fill_rounded(pill, pill.height() / 2.0, theme.badge_bg);
        let label = RectF::new(pill.left + padding, pill.top, pill.right, pill.bottom);
        canvas.text(badge, TextStyle::Footer, label, theme.badge_text);
        status_right = pill.left - padding;
    }
    if let Some(status) = frame.footer_status {
        let label = RectF::new(layout.icon_left, top + 1.0, status_right, layout.height);
        canvas.text(status, TextStyle::Footer, label, theme.shortcut_text);
    }
}

fn utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// Byte stride of a premultiplied BGRA icon, or `None` when its buffer is too small to read safely.
fn icon_stride(icon: &BgraIcon) -> Option<u32> {
    let stride = icon.width.checked_mul(4)?;
    let required = (stride as usize).checked_mul(icon.height as usize)?;
    (icon.width > 0 && icon.height > 0 && icon.pixels.len() >= required).then_some(stride)
}

fn icon_key(icon: &Arc<BgraIcon>) -> usize {
    Arc::as_ptr(icon) as usize
}

fn d2d_color(color: Color) -> D2D1_COLOR_F {
    let [r, g, b, a] = color.to_unit_rgba();
    D2D1_COLOR_F { r, g, b, a }
}

fn colorref(color: Color) -> COLORREF {
    COLORREF(color.to_colorref())
}

const LOCALE_NAME_MAX_LENGTH: usize = 85;

/// DirectWrite picks CJK fallback fonts by locale, so Japanese labels need the user's locale
/// to render with Japanese glyph forms.
fn user_locale() -> HSTRING {
    let mut buffer = [0u16; LOCALE_NAME_MAX_LENGTH];
    let length = unsafe { GetUserDefaultLocaleName(&mut buffer) };
    if length > 1 {
        HSTRING::from(String::from_utf16_lossy(&buffer[..length as usize - 1]))
    } else {
        HSTRING::from("en-US")
    }
}

struct TextFormats {
    label: IDWriteTextFormat,
    bold: IDWriteTextFormat,
    shortcut: IDWriteTextFormat,
    footer: IDWriteTextFormat,
    glyph: IDWriteTextFormat,
}

impl TextFormats {
    fn new(dwrite: &IDWriteFactory, family: &str, metrics: &MenuMetrics) -> WinResult<Self> {
        let family = HSTRING::from(family);
        let locale = user_locale();
        let size = metrics.font_size;
        let format = |weight, size, alignment, ellipsis| {
            text_format(dwrite, &family, &locale, weight, size, alignment, ellipsis)
        };
        Ok(Self {
            label: format(
                DWRITE_FONT_WEIGHT_NORMAL,
                size,
                DWRITE_TEXT_ALIGNMENT_LEADING,
                true,
            )?,
            bold: format(
                DWRITE_FONT_WEIGHT_BOLD,
                size,
                DWRITE_TEXT_ALIGNMENT_LEADING,
                true,
            )?,
            shortcut: format(
                DWRITE_FONT_WEIGHT_NORMAL,
                size,
                DWRITE_TEXT_ALIGNMENT_TRAILING,
                false,
            )?,
            footer: format(
                DWRITE_FONT_WEIGHT_NORMAL,
                size * FOOTER_FONT_RATIO,
                DWRITE_TEXT_ALIGNMENT_LEADING,
                false,
            )?,
            glyph: format(
                DWRITE_FONT_WEIGHT_NORMAL,
                size * GLYPH_FONT_RATIO,
                DWRITE_TEXT_ALIGNMENT_CENTER,
                false,
            )?,
        })
    }

    fn get(&self, style: TextStyle) -> &IDWriteTextFormat {
        match style {
            TextStyle::Label => &self.label,
            TextStyle::Bold => &self.bold,
            TextStyle::Shortcut => &self.shortcut,
            TextStyle::Footer => &self.footer,
            TextStyle::Glyph => &self.glyph,
        }
    }
}

fn text_format(
    dwrite: &IDWriteFactory,
    family: &HSTRING,
    locale: &HSTRING,
    weight: DWRITE_FONT_WEIGHT,
    size: f32,
    alignment: DWRITE_TEXT_ALIGNMENT,
    ellipsis: bool,
) -> WinResult<IDWriteTextFormat> {
    unsafe {
        let format = dwrite.CreateTextFormat(
            family,
            None,
            weight,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            locale,
        )?;
        format.SetTextAlignment(alignment)?;
        format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
        format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
        if ellipsis {
            let sign = dwrite.CreateEllipsisTrimmingSign(&format)?;
            let trimming = DWRITE_TRIMMING {
                granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                delimiter: 0,
                delimiterCount: 0,
            };
            format.SetTrimming(&trimming, &sign)?;
        }
        Ok(format)
    }
}

fn measure_with_dwrite(dwrite: &IDWriteFactory, format: &IDWriteTextFormat, text: &str) -> f32 {
    let measured = unsafe {
        dwrite
            .CreateTextLayout(&utf16(text), format, 8192.0, 1024.0)
            .and_then(|layout| {
                let mut metrics = DWRITE_TEXT_METRICS::default();
                layout
                    .GetMetrics(&mut metrics)
                    .map(|()| metrics.widthIncludingTrailingWhitespace)
            })
    };
    measured.unwrap_or_else(|_| text.chars().count() as f32 * unsafe { format.GetFontSize() } * 0.6)
}

struct CachedBitmap {
    _icon: Arc<BgraIcon>,
    bitmap: Option<ID2D1Bitmap>,
}

/// Device-dependent resources. Dropped and rebuilt when `EndDraw` reports `D2DERR_RECREATE_TARGET`.
struct D2dTarget {
    rt: ID2D1HwndRenderTarget,
    brush: ID2D1SolidColorBrush,
    bitmaps: HashMap<usize, CachedBitmap>,
}

struct D2dBackend {
    factory: ID2D1Factory,
    dwrite: IDWriteFactory,
    formats: TextFormats,
    hwnd: HWND,
    size: D2D_SIZE_U,
    target: Option<D2dTarget>,
    failures: u8,
}

impl D2dBackend {
    fn new(hwnd: HWND, metrics: &MenuMetrics, family: &str) -> WinResult<Self> {
        // Owned per renderer, never cached in a `thread_local!`: Rust runs TLS destructors under
        // the loader lock, and releasing Direct2D or DirectWrite there deadlocks thread exit.
        let (factory, dwrite) = create_factories()?;
        let formats = TextFormats::new(&dwrite, family, metrics)?;
        let mut client = RECT::default();
        unsafe { GetClientRect(hwnd, &mut client)? };
        let size = D2D_SIZE_U {
            width: (client.right - client.left).max(1) as u32,
            height: (client.bottom - client.top).max(1) as u32,
        };
        let target = create_target(&factory, hwnd, size)?;
        Ok(Self {
            factory,
            dwrite,
            formats,
            hwnd,
            size,
            target: Some(target),
            failures: 0,
        })
    }

    fn render(
        &mut self,
        frame: &MenuFrame,
        theme: &ThemeColors,
        metrics: &MenuMetrics,
    ) -> WinResult<()> {
        if self.target.is_none() {
            self.target = Some(create_target(&self.factory, self.hwnd, self.size)?);
        }
        let Some(target) = self.target.as_mut() else {
            return Ok(());
        };
        let mut canvas = D2dCanvas {
            rt: &target.rt,
            brush: &target.brush,
            formats: &self.formats,
            dwrite: &self.dwrite,
            bitmaps: &mut target.bitmaps,
        };
        unsafe {
            target.rt.BeginDraw();
            draw_frame(&mut canvas, frame, theme, metrics);
            target.rt.EndDraw(None, None)?;
        }
        self.failures = 0;
        Ok(())
    }
}

fn create_target(factory: &ID2D1Factory, hwnd: HWND, size: D2D_SIZE_U) -> WinResult<D2dTarget> {
    let properties = D2D1_RENDER_TARGET_PROPERTIES {
        // Software rasterization avoids ~200 ms per-popup DXGI/D3D11 swapchain init while
        // keeping full Direct2D + DirectWrite ClearType rendering inside explorer.exe.
        r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
        // The popup is opaque; ignoring alpha keeps ClearType text available.
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_IGNORE,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        usage: D2D1_RENDER_TARGET_USAGE_NONE,
        minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
    };
    let hwnd_properties = D2D1_HWND_RENDER_TARGET_PROPERTIES {
        hwnd,
        pixelSize: size,
        presentOptions: D2D1_PRESENT_OPTIONS_IMMEDIATELY,
    };
    unsafe {
        let rt = factory.CreateHwndRenderTarget(&properties, &hwnd_properties)?;
        let brush = rt.CreateSolidColorBrush(&d2d_color(Color::from_hex(0)), None)?;
        Ok(D2dTarget {
            rt,
            brush,
            bitmaps: HashMap::new(),
        })
    }
}

fn create_bitmap(rt: &ID2D1HwndRenderTarget, icon: &BgraIcon) -> Option<ID2D1Bitmap> {
    let stride = icon_stride(icon)?;
    let properties = D2D1_BITMAP_PROPERTIES {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
    };
    let size = D2D_SIZE_U {
        width: icon.width,
        height: icon.height,
    };
    unsafe {
        rt.CreateBitmap(size, Some(icon.pixels.as_ptr().cast()), stride, &properties)
            .ok()
    }
}

struct D2dCanvas<'a> {
    rt: &'a ID2D1HwndRenderTarget,
    brush: &'a ID2D1SolidColorBrush,
    formats: &'a TextFormats,
    dwrite: &'a IDWriteFactory,
    bitmaps: &'a mut HashMap<usize, CachedBitmap>,
}

impl D2dCanvas<'_> {
    fn paint_with(&self, color: Color) -> &ID2D1SolidColorBrush {
        unsafe { self.brush.SetColor(&d2d_color(color)) };
        self.brush
    }
}

impl Canvas for D2dCanvas<'_> {
    fn clear(&mut self, color: Color) {
        unsafe { self.rt.Clear(Some(&d2d_color(color))) };
    }

    fn fill_rect(&mut self, rect: RectF, color: Color) {
        let brush = self.paint_with(color);
        unsafe { self.rt.FillRectangle(&rect.to_d2d(), brush) };
    }

    fn fill_rounded(&mut self, rect: RectF, radius: f32, color: Color) {
        let brush = self.paint_with(color);
        let rounded = D2D1_ROUNDED_RECT {
            rect: rect.to_d2d(),
            radiusX: radius,
            radiusY: radius,
        };
        unsafe { self.rt.FillRoundedRectangle(&rounded, brush) };
    }

    fn stroke_rounded(&mut self, rect: RectF, radius: f32, color: Color) {
        let brush = self.paint_with(color);
        // A 1px stroke centred half a pixel inside the edge covers exactly one pixel column.
        let edge = rect.inset(0.5);
        unsafe {
            if radius <= 0.5 {
                self.rt.DrawRectangle(&edge.to_d2d(), brush, 1.0, None);
            } else {
                let rounded = D2D1_ROUNDED_RECT {
                    rect: edge.to_d2d(),
                    radiusX: radius - 0.5,
                    radiusY: radius - 0.5,
                };
                self.rt.DrawRoundedRectangle(&rounded, brush, 1.0, None);
            }
        }
    }

    fn text(&mut self, text: &str, style: TextStyle, rect: RectF, color: Color) {
        let brush = self.paint_with(color);
        unsafe {
            self.rt.DrawText(
                &utf16(text),
                self.formats.get(style),
                &rect.to_d2d(),
                brush,
                D2D1_DRAW_TEXT_OPTIONS_CLIP | D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    fn icon(&mut self, icon: &Arc<BgraIcon>, rect: RectF, opacity: f32) {
        let rt = self.rt;
        let cached = self
            .bitmaps
            .entry(icon_key(icon))
            .or_insert_with(|| CachedBitmap {
                _icon: Arc::clone(icon),
                bitmap: create_bitmap(rt, icon),
            });
        if let Some(bitmap) = &cached.bitmap {
            unsafe {
                rt.DrawBitmap(
                    bitmap,
                    Some(&rect.to_d2d()),
                    opacity,
                    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                    None,
                );
            }
        }
    }

    fn measure(&mut self, text: &str, style: TextStyle) -> f32 {
        measure_with_dwrite(self.dwrite, self.formats.get(style), text)
    }

    fn push_clip(&mut self, rect: RectF) {
        unsafe {
            self.rt
                .PushAxisAlignedClip(&rect.to_d2d(), D2D1_ANTIALIAS_MODE_ALIASED)
        };
    }

    fn pop_clip(&mut self) {
        unsafe { self.rt.PopAxisAlignedClip() };
    }
}

struct GdiFonts {
    label: HFONT,
    bold: HFONT,
    footer: HFONT,
    glyph: HFONT,
}

impl GdiFonts {
    fn new(family: &str, metrics: &MenuMetrics) -> Self {
        let size = metrics.font_size;
        Self {
            label: create_gdi_font(family, size, FW_NORMAL),
            bold: create_gdi_font(family, size, FW_BOLD),
            footer: create_gdi_font(family, size * FOOTER_FONT_RATIO, FW_NORMAL),
            glyph: create_gdi_font(family, size * GLYPH_FONT_RATIO, FW_NORMAL),
        }
    }

    fn get(&self, style: TextStyle) -> HFONT {
        match style {
            TextStyle::Label | TextStyle::Shortcut => self.label,
            TextStyle::Bold => self.bold,
            TextStyle::Footer => self.footer,
            TextStyle::Glyph => self.glyph,
        }
    }
}

impl Drop for GdiFonts {
    fn drop(&mut self) {
        for font in [self.label, self.bold, self.footer, self.glyph] {
            unsafe {
                let _ = DeleteObject(font);
            }
        }
    }
}

fn create_gdi_font(family: &str, pixel_size: f32, weight: FONT_WEIGHT) -> HFONT {
    let mut logfont = LOGFONTW {
        lfHeight: -(pixel_size.round() as i32),
        lfWeight: weight.0 as i32,
        lfCharSet: DEFAULT_CHARSET,
        lfQuality: CLEARTYPE_QUALITY,
        ..Default::default()
    };
    let face = logfont.lfFaceName.len() - 1;
    for (slot, unit) in logfont
        .lfFaceName
        .iter_mut()
        .zip(family.encode_utf16().take(face))
    {
        *slot = unit;
    }
    unsafe { CreateFontIndirectW(&logfont) }
}

fn measure_with_gdi(dc: HDC, font: HFONT, text: &str) -> f32 {
    let mut size = SIZE::default();
    unsafe {
        let previous = SelectObject(dc, font);
        let _ = GetTextExtentPoint32W(dc, &utf16(text), &mut size);
        SelectObject(dc, previous);
    }
    size.cx as f32
}

/// Premultiplied 32bpp DIB section for one icon.
struct Dib(HBITMAP);

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.0);
        }
    }
}

struct CachedDib {
    _icon: Arc<BgraIcon>,
    dib: Option<Dib>,
}

fn create_dib(icon: &BgraIcon) -> Option<Dib> {
    let stride = icon_stride(icon)?;
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: icon.width as i32,
            biHeight: -(icon.height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    unsafe {
        let bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let dib = Dib(bitmap);
        if bits.is_null() {
            return None;
        }
        let len = stride as usize * icon.height as usize;
        std::ptr::copy_nonoverlapping(icon.pixels.as_ptr(), bits.cast::<u8>(), len);
        Some(dib)
    }
}

struct GdiBackend {
    fonts: GdiFonts,
    dibs: HashMap<usize, CachedDib>,
    measure_dc: HDC,
}

impl GdiBackend {
    fn new(metrics: &MenuMetrics, family: &str) -> Self {
        Self {
            fonts: GdiFonts::new(family, metrics),
            dibs: HashMap::new(),
            measure_dc: unsafe { CreateCompatibleDC(None) },
        }
    }

    fn render(
        &mut self,
        target: HDC,
        frame: &MenuFrame,
        theme: &ThemeColors,
        metrics: &MenuMetrics,
    ) {
        let (width, height) = frame.layout.pixel_size();
        unsafe {
            let buffer = CreateCompatibleDC(target);
            let bitmap = CreateCompatibleBitmap(target, width, height);
            let previous = SelectObject(buffer, bitmap);
            SetBkMode(buffer, TRANSPARENT);
            let mut canvas = GdiCanvas {
                dc: buffer,
                size: (width, height),
                fonts: &self.fonts,
                dibs: &mut self.dibs,
                saved: Vec::new(),
            };
            draw_frame(&mut canvas, frame, theme, metrics);
            let _ = BitBlt(target, 0, 0, width, height, buffer, 0, 0, SRCCOPY);
            SelectObject(buffer, previous);
            let _ = DeleteObject(bitmap);
            let _ = DeleteDC(buffer);
        }
    }
}

impl Drop for GdiBackend {
    fn drop(&mut self) {
        self.dibs.clear();
        unsafe {
            let _ = DeleteDC(self.measure_dc);
        }
    }
}

struct GdiCanvas<'a> {
    dc: HDC,
    size: (i32, i32),
    fonts: &'a GdiFonts,
    dibs: &'a mut HashMap<usize, CachedDib>,
    saved: Vec<i32>,
}

impl GdiCanvas<'_> {
    fn round_rect(&mut self, rect: RectF, radius: f32, fill: Option<Color>, stroke: Color) {
        let r = rect.to_gdi();
        let diameter = (radius * 2.0).round() as i32;
        unsafe {
            let pen = CreatePen(PS_SOLID, 1, colorref(stroke));
            let brush = fill.map(|color| CreateSolidBrush(colorref(color)));
            let previous_pen = SelectObject(self.dc, pen);
            let previous_brush = match brush {
                Some(brush) => SelectObject(self.dc, brush),
                None => SelectObject(self.dc, GetStockObject(NULL_BRUSH)),
            };
            let _ = RoundRect(
                self.dc, r.left, r.top, r.right, r.bottom, diameter, diameter,
            );
            SelectObject(self.dc, previous_brush);
            SelectObject(self.dc, previous_pen);
            let _ = DeleteObject(pen);
            if let Some(brush) = brush {
                let _ = DeleteObject(brush);
            }
        }
    }
}

impl Canvas for GdiCanvas<'_> {
    fn clear(&mut self, color: Color) {
        let (width, height) = self.size;
        self.fill_rect(RectF::new(0.0, 0.0, width as f32, height as f32), color);
    }

    fn fill_rect(&mut self, rect: RectF, color: Color) {
        unsafe {
            let brush = CreateSolidBrush(colorref(color));
            FillRect(self.dc, &rect.to_gdi(), brush);
            let _ = DeleteObject(brush);
        }
    }

    fn fill_rounded(&mut self, rect: RectF, radius: f32, color: Color) {
        self.round_rect(rect, radius, Some(color), color);
    }

    fn stroke_rounded(&mut self, rect: RectF, radius: f32, color: Color) {
        self.round_rect(rect, radius, None, color);
    }

    fn text(&mut self, text: &str, style: TextStyle, rect: RectF, color: Color) {
        let align = match style {
            TextStyle::Shortcut => DT_RIGHT,
            TextStyle::Glyph => DT_CENTER,
            TextStyle::Label | TextStyle::Bold | TextStyle::Footer => DT_LEFT | DT_END_ELLIPSIS,
        };
        let format: DRAW_TEXT_FORMAT = align | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX;
        let mut buffer = utf16(text);
        let mut bounds = rect.to_gdi();
        unsafe {
            let previous = SelectObject(self.dc, self.fonts.get(style));
            SetTextColor(self.dc, colorref(color));
            DrawTextW(self.dc, &mut buffer, &mut bounds, format);
            SelectObject(self.dc, previous);
        }
    }

    fn icon(&mut self, icon: &Arc<BgraIcon>, rect: RectF, opacity: f32) {
        let cached = self
            .dibs
            .entry(icon_key(icon))
            .or_insert_with(|| CachedDib {
                _icon: Arc::clone(icon),
                dib: create_dib(icon),
            });
        let Some(dib) = &cached.dib else {
            return;
        };
        let r = rect.to_gdi();
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: (opacity.clamp(0.0, 1.0) * 255.0).round() as u8,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        unsafe {
            let source = CreateCompatibleDC(self.dc);
            let previous = SelectObject(source, dib.0);
            let _ = GdiAlphaBlend(
                self.dc,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                source,
                0,
                0,
                icon.width as i32,
                icon.height as i32,
                blend,
            );
            SelectObject(source, previous);
            let _ = DeleteDC(source);
        }
    }

    fn measure(&mut self, text: &str, style: TextStyle) -> f32 {
        measure_with_gdi(self.dc, self.fonts.get(style), text)
    }

    fn push_clip(&mut self, rect: RectF) {
        let r = rect.to_gdi();
        unsafe {
            self.saved.push(SaveDC(self.dc));
            IntersectClipRect(self.dc, r.left, r.top, r.right, r.bottom);
        }
    }

    fn pop_clip(&mut self) {
        if let Some(saved) = self.saved.pop() {
            unsafe {
                let _ = RestoreDC(self.dc, saved);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::compute_metrics;
    use rwm_core::config::Density;

    const WORK: PixelRect = PixelRect::new(0, 0, 1920, 1040);

    fn classic(dpi: u32) -> MenuMetrics {
        compute_metrics(Density::Classic, dpi)
    }

    fn item(label_width: f32, shortcut_width: f32) -> RowSpec {
        RowSpec::Item {
            label_width,
            shortcut_width,
        }
    }

    fn items(count: usize) -> Vec<RowSpec> {
        (0..count).map(|_| item(80.0, 0.0)).collect()
    }

    fn natural(specs: &[RowSpec], metrics: &MenuMetrics) -> MenuLayout {
        compute_layout(specs, metrics, false, f32::INFINITY)
    }

    #[test]
    fn rows_stack_top_to_bottom_without_gaps() {
        let m = classic(96);
        let specs = [
            item(60.0, 0.0),
            RowSpec::Separator,
            item(60.0, 0.0),
            item(60.0, 0.0),
        ];
        let layout = natural(&specs, &m);

        assert_eq!(layout.rows.len(), specs.len());
        assert_eq!(layout.rows[0].top, m.padding_y);
        for pair in layout.rows.windows(2) {
            assert_eq!(pair[1].top, pair[0].top + pair[0].height);
        }
        assert!(layout.rows[1].is_separator);
        assert_eq!(layout.rows[1].height, m.separator_height);
        assert_eq!(layout.rows[0].height, m.row_height);
        let expected = 2.0 * m.padding_y + 3.0 * m.row_height + m.separator_height;
        assert_eq!(layout.height, expected);
        assert_eq!(layout.content_height, expected);
        assert!(!layout.is_scrollable());
    }

    #[test]
    fn footer_sits_below_the_rows() {
        let m = classic(96);
        let without = natural(&items(3), &m);
        let with = compute_layout(&items(3), &m, true, f32::INFINITY);

        assert_eq!(with.height, without.height + m.footer_height);
        assert_eq!(with.footer_top(), without.height);
        assert_eq!(
            with.hit_test(10.0, with.height - 1.0, 0.0),
            HitTarget::Footer
        );
        assert_eq!(without.footer_height, 0.0);
    }

    #[test]
    fn width_grows_so_the_widest_label_fits_unclipped() {
        let m = classic(96);
        let narrow = natural(&[item(40.0, 0.0), item(60.0, 0.0)], &m);
        let wide = natural(&[item(40.0, 0.0), item(320.0, 0.0)], &m);

        assert!(wide.width > narrow.width);
        assert!(wide.label_left + 320.0 <= wide.rows[1].label_right);
        assert!(wide.rows[1].label_right <= wide.chevron_left);
        assert!(wide.chevron_right <= wide.pill_right);
        assert!(wide.pill_right < wide.width);
    }

    #[test]
    fn shortcuts_share_one_right_aligned_column_beside_their_labels() {
        let m = classic(96);
        let specs = [item(150.0, 40.0), item(90.0, 72.0), item(200.0, 0.0)];
        let layout = natural(&specs, &m);

        for (spec, row) in specs.iter().zip(&layout.rows) {
            let RowSpec::Item {
                label_width,
                shortcut_width,
            } = *spec
            else {
                unreachable!()
            };
            assert!(
                layout.label_left + label_width <= row.label_right,
                "label must fit before its shortcut"
            );
            assert!(
                row.label_right + shortcut_width <= layout.shortcut_right,
                "shortcut must fit before the shared right edge"
            );
        }
        assert!(layout.shortcut_right <= layout.chevron_left);
        assert_eq!(layout.rows[2].label_right, layout.chevron_left);
    }

    #[test]
    fn width_is_clamped_between_minimum_and_maximum() {
        let m = classic(96);
        let empty = natural(&[], &m);
        let tiny = natural(&[item(1.0, 0.0)], &m);
        let huge = natural(&[item(5000.0, 3000.0)], &m);
        let wider_still = natural(&[item(9000.0, 3000.0)], &m);

        assert_eq!(
            tiny.width, empty.width,
            "short menus share the minimum width"
        );
        assert_eq!(
            huge.width, wider_still.width,
            "long labels stop growing the menu"
        );
        assert!(huge.width < 1000.0);
        assert!(huge.rows[0].label_right > huge.label_left);
        assert_eq!(empty.height, 2.0 * m.padding_y);
        assert_eq!(empty.width, empty.width.round());
    }

    #[test]
    fn tall_menus_scroll_inside_the_maximum_height() {
        let m = classic(96);
        let layout = compute_layout(&items(60), &m, true, 400.0);

        assert!(layout.is_scrollable());
        assert_eq!(layout.height, 400.0);
        assert!(layout.scroll_button_height > 0.0);
        assert_eq!(layout.viewport_top(), layout.scroll_button_height);
        assert_eq!(
            layout.viewport_bottom(),
            400.0 - m.footer_height - layout.scroll_button_height
        );
        let view = layout.viewport_bottom() - layout.viewport_top();
        assert_eq!(layout.max_scroll(), layout.content_height - view);

        assert_eq!(layout.hit_test(50.0, 1.0, 0.0), HitTarget::ScrollUp);
        assert_eq!(
            layout.hit_test(50.0, layout.viewport_bottom() + 1.0, 0.0),
            HitTarget::ScrollDown
        );
        let first_row_y = layout.viewport_top() + m.padding_y + 1.0;
        assert_eq!(layout.hit_test(50.0, first_row_y, 0.0), HitTarget::Row(0));
        let bottom_y = layout.viewport_bottom() - m.padding_y - 1.0;
        assert_eq!(
            layout.hit_test(50.0, bottom_y, layout.max_scroll()),
            HitTarget::Row(59)
        );
        assert_eq!(layout.clamp_scroll(1e9), layout.max_scroll());
        assert_eq!(layout.clamp_scroll(-5.0), 0.0);
    }

    #[test]
    fn short_menus_never_scroll() {
        let m = classic(96);
        let layout = compute_layout(&items(5), &m, true, 1040.0);
        assert!(!layout.is_scrollable());
        assert_eq!(layout.max_scroll(), 0.0);
        assert_eq!(layout.scroll_to_reveal(4, 0.0), 0.0);
    }

    #[test]
    fn hit_test_maps_points_to_rows() {
        let m = classic(96);
        let specs = [item(60.0, 0.0), RowSpec::Separator, item(60.0, 0.0)];
        let layout = natural(&specs, &m);
        let middle = |row: usize| layout.rows[row].top + layout.rows[row].height / 2.0;

        assert_eq!(layout.hit_test(20.0, middle(0), 0.0), HitTarget::Row(0));
        assert_eq!(layout.hit_test(20.0, middle(1), 0.0), HitTarget::Row(1));
        assert_eq!(layout.hit_test(20.0, middle(2), 0.0), HitTarget::Row(2));
        assert_eq!(
            layout.hit_test(20.0, 1.0, 0.0),
            HitTarget::Nothing,
            "top padding"
        );
        assert_eq!(layout.hit_test(-1.0, middle(0), 0.0), HitTarget::Nothing);
        assert_eq!(
            layout.hit_test(layout.width, middle(0), 0.0),
            HitTarget::Nothing
        );
        assert_eq!(
            layout.hit_test(20.0, layout.height, 0.0),
            HitTarget::Nothing
        );
        let boundary = layout.rows[2].top;
        assert_eq!(layout.hit_test(20.0, boundary, 0.0), HitTarget::Row(2));
        assert_eq!(
            layout.hit_test(20.0, boundary - 0.01, 0.0),
            HitTarget::Row(1)
        );
    }

    #[test]
    fn scroll_to_reveal_brings_rows_into_view() {
        let m = classic(96);
        let layout = compute_layout(&items(60), &m, false, 400.0);
        let view = layout.viewport_bottom() - layout.viewport_top();

        assert_eq!(layout.scroll_to_reveal(59, 0.0), layout.max_scroll());
        assert_eq!(layout.scroll_to_reveal(0, layout.max_scroll()), 0.0);
        assert_eq!(
            layout.scroll_to_reveal(2, 0.0),
            0.0,
            "visible rows stay put"
        );

        let row = 30;
        let revealed = layout.scroll_to_reveal(row, 0.0);
        let r = layout.rows[row];
        assert_eq!(revealed, r.top + r.height - view);
        let top = layout.row_top(row, revealed);
        assert!(top >= layout.viewport_top());
        assert!(top + r.height <= layout.viewport_bottom());

        let back = layout.scroll_to_reveal(10, revealed);
        assert_eq!(layout.row_top(10, back), layout.viewport_top());
    }

    #[test]
    fn layout_scales_with_dpi() {
        let one = classic(96);
        let two = classic(192);
        let specs_96 = [item(100.0, 30.0), RowSpec::Separator, item(140.0, 0.0)];
        let specs_192 = [item(200.0, 60.0), RowSpec::Separator, item(280.0, 0.0)];
        let a = natural(&specs_96, &one);
        let b = natural(&specs_192, &two);

        assert!((b.width - 2.0 * a.width).abs() <= 1.0);
        assert_eq!(b.height, 2.0 * a.height);
        assert_eq!(b.label_left, 2.0 * a.label_left);
        assert_eq!(b.rows[2].top, 2.0 * a.rows[2].top);
    }

    #[test]
    fn root_popup_opens_at_the_anchor_when_it_fits() {
        assert_eq!(clamp_popup_origin((500, 300), (240, 400), WORK), (500, 300));
    }

    #[test]
    fn root_popup_flips_left_near_the_right_edge() {
        assert_eq!(
            clamp_popup_origin((1800, 300), (240, 400), WORK),
            (1560, 300)
        );
    }

    #[test]
    fn root_popup_flips_up_near_the_bottom_edge() {
        assert_eq!(clamp_popup_origin((500, 900), (240, 400), WORK), (500, 500));
    }

    #[test]
    fn root_popup_flips_both_ways_in_the_bottom_right_corner() {
        assert_eq!(
            clamp_popup_origin((1900, 1000), (240, 400), WORK),
            (1660, 600)
        );
    }

    #[test]
    fn root_popup_stays_inside_when_flipping_overshoots() {
        let narrow = PixelRect::new(0, 0, 300, 500);
        assert_eq!(clamp_popup_origin((250, 10), (280, 100), narrow), (0, 10));
        assert_eq!(clamp_popup_origin((10, 450), (100, 480), narrow), (10, 0));
    }

    #[test]
    fn oversized_popup_is_pinned_to_the_work_area_origin() {
        let (x, y) = clamp_popup_origin((700, 700), (2500, 1600), WORK);
        assert_eq!((x, y), (WORK.left, WORK.top));
    }

    #[test]
    fn popup_on_a_left_monitor_with_negative_coordinates_stays_inside() {
        let left = PixelRect::new(-1920, 0, 0, 1040);
        assert_eq!(
            clamp_popup_origin((-10, 1030), (200, 300), left),
            (-210, 730)
        );
        assert_eq!(clamp_popup_origin((-1915, 5), (200, 300), left), (-1915, 5));
    }

    #[test]
    fn anchor_over_the_taskbar_still_lands_inside_the_work_area() {
        let (x, y) = clamp_popup_origin((500, 1060), (200, 300), WORK);
        assert_eq!((x, y), (500, WORK.bottom - 300));
        let (_, y) = clamp_popup_origin((500, 1400), (200, 300), WORK);
        assert_eq!(y, WORK.bottom - 300);
    }

    #[test]
    fn submenu_opens_to_the_right_level_with_its_owner_row() {
        let owner = PixelRect::new(100, 200, 300, 226);
        assert_eq!(
            clamp_submenu_origin(owner, (200, 150), WORK, 4, 4),
            (296, 196)
        );
    }

    #[test]
    fn submenu_opens_to_the_left_without_room_on_the_right() {
        let owner = PixelRect::new(1700, 200, 1900, 226);
        assert_eq!(
            clamp_submenu_origin(owner, (200, 150), WORK, 4, 4),
            (1504, 196)
        );
    }

    #[test]
    fn submenu_aligns_its_bottom_without_room_below() {
        let owner = PixelRect::new(100, 1000, 300, 1026);
        let (_, y) = clamp_submenu_origin(owner, (200, 150), WORK, 4, 4);
        assert_eq!(y, 1026 + 4 - 150);
        let tall = clamp_submenu_origin(owner, (200, 2000), WORK, 4, 4);
        assert_eq!(tall.1, WORK.top);
    }

    #[test]
    fn growing_menu_keeps_its_origin_while_it_fits() {
        let current = PixelRect::from_origin_size((500, 300), (240, 400));
        assert_eq!(
            reflow_origin(current, (500, 300), (260, 480), WORK),
            (500, 300)
        );
    }

    #[test]
    fn growing_menu_shifts_up_only_by_its_overflow() {
        let current = PixelRect::from_origin_size((500, 600), (240, 400));
        assert_eq!(
            reflow_origin(current, (500, 600), (240, 480), WORK),
            (500, 560)
        );
    }

    #[test]
    fn growing_upward_menu_keeps_its_bottom_edge() {
        let anchor = (500, 900);
        let current = PixelRect::from_origin_size((500, 500), (240, 400));
        assert_eq!(reflow_origin(current, anchor, (240, 480), WORK), (500, 420));
    }

    #[test]
    fn growing_leftward_menu_keeps_its_right_edge() {
        let anchor = (1800, 300);
        let current = PixelRect::from_origin_size((1560, 300), (240, 400));
        assert_eq!(
            reflow_origin(current, anchor, (300, 400), WORK),
            (1500, 300)
        );
    }

    #[test]
    fn growing_menu_never_leaves_the_work_area() {
        let current = PixelRect::from_origin_size((10, 10), (240, 400));
        let (x, y) = reflow_origin(current, (10, 10), (240, 5000), WORK);
        assert_eq!((x, y), (10, WORK.top));
    }

    #[test]
    fn anchored_reflow_pins_hovered_row_or_clamps_to_work_area() {
        // Upward menu where Copy was at screen y = 550 (top = 500, row_top = 50);
        // after inline insertion Copy moves down inside the menu to new_row_top = 130.
        let current = PixelRect::from_origin_size((500, 500), (240, 400));
        assert_eq!(
            reflow_origin_anchored(current, (500, 900), (240, 480), WORK, Some((550.0, 130.0))),
            (500, 420)
        );
        // Downward menu over Properties: Properties was at screen y = 650 (new_row_top = 430);
        // keeping Properties under the cursor moves top to 220 while room permits.
        assert_eq!(
            reflow_origin_anchored(current, (500, 500), (240, 480), WORK, Some((650.0, 430.0))),
            (500, 220)
        );
        // Footer disappearing shifts rows only if unpinned; pinned row keeps desired screen top.
        let small_work = PixelRect::new(0, 200, 1920, 680);
        assert_eq!(
            reflow_origin_anchored(
                PixelRect::from_origin_size((500, 260), (240, 400)),
                (500, 260),
                (240, 480),
                small_work,
                Some((620.0, 450.0)),
            ),
            (500, 200)
        );
    }

    #[test]
    fn icon_buffers_too_small_for_their_size_are_rejected() {
        let icon = |width, height, len| BgraIcon {
            width,
            height,
            pixels: vec![0; len],
        };
        assert_eq!(icon_stride(&icon(16, 16, 16 * 16 * 4)), Some(64));
        assert_eq!(icon_stride(&icon(16, 16, 16 * 16 * 4 - 1)), None);
        assert_eq!(icon_stride(&icon(0, 16, 64)), None);
        assert_eq!(icon_stride(&icon(u32::MAX, 2, 8)), None);
    }
}
