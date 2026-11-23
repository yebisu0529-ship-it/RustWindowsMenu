use rwm_core::config::{Density, ThemePreference};
use windows::core::w;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};

/// Opaque sRGB color. The palette stays opaque because the GDI fallback cannot blend fills.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const fn from_hex(rgb: u32) -> Self {
        Self {
            r: (rgb >> 16) as u8,
            g: (rgb >> 8) as u8,
            b: rgb as u8,
        }
    }

    /// GDI `COLORREF` layout (`0x00BBGGRR`).
    pub const fn to_colorref(self) -> u32 {
        self.r as u32 | (self.g as u32) << 8 | (self.b as u32) << 16
    }

    pub fn to_unit_rgba(self) -> [f32; 4] {
        [
            f32::from(self.r) / 255.0,
            f32::from(self.g) / 255.0,
            f32::from(self.b) / 255.0,
            1.0,
        ]
    }

    /// WCAG 2.x relative luminance.
    pub fn relative_luminance(self) -> f64 {
        fn linear(channel: u8) -> f64 {
            let c = f64::from(channel) / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * linear(self.r) + 0.7152 * linear(self.g) + 0.0722 * linear(self.b)
    }

    /// WCAG 2.x contrast ratio, from 1.0 (identical) to 21.0 (black on white).
    pub fn contrast_ratio(self, other: Color) -> f64 {
        let (a, b) = (self.relative_luminance(), other.relative_luminance());
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeColors {
    pub is_dark: bool,
    pub bg: Color,
    pub border: Color,
    pub text: Color,
    pub text_disabled: Color,
    pub shortcut_text: Color,
    pub hover_bg: Color,
    pub hover_border: Color,
    pub separator: Color,
    pub badge_bg: Color,
    pub badge_text: Color,
    pub chevron: Color,
    pub footer_bg: Color,
}

impl ThemeColors {
    pub fn light() -> Self {
        Self {
            is_dark: false,
            bg: Color::from_hex(0xF9F9F9),
            border: Color::from_hex(0xCFCFCF),
            text: Color::from_hex(0x1A1A1A),
            text_disabled: Color::from_hex(0xA0A0A0),
            shortcut_text: Color::from_hex(0x6B6B6B),
            hover_bg: Color::from_hex(0xEAEAEA),
            hover_border: Color::from_hex(0xE0E0E0),
            separator: Color::from_hex(0xE2E2E2),
            badge_bg: Color::from_hex(0xDCE7F7),
            badge_text: Color::from_hex(0x0F4C81),
            chevron: Color::from_hex(0x5C5C5C),
            footer_bg: Color::from_hex(0xF1F1F1),
        }
    }

    pub fn dark() -> Self {
        Self {
            is_dark: true,
            bg: Color::from_hex(0x2C2C2C),
            border: Color::from_hex(0x474747),
            text: Color::from_hex(0xF3F3F3),
            text_disabled: Color::from_hex(0x7A7A7A),
            shortcut_text: Color::from_hex(0xA8A8A8),
            hover_bg: Color::from_hex(0x3A3A3A),
            hover_border: Color::from_hex(0x444444),
            separator: Color::from_hex(0x404040),
            badge_bg: Color::from_hex(0x1F3A5C),
            badge_text: Color::from_hex(0xCFE3FF),
            chevron: Color::from_hex(0xC8C8C8),
            footer_bg: Color::from_hex(0x262626),
        }
    }
}

/// Geometry in physical pixels for one monitor DPI. The renderer keeps its target at 96 DPI so one
/// drawing unit equals one pixel, which lets layout, hit testing, and both backends share numbers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MenuMetrics {
    pub row_height: f32,
    pub separator_height: f32,
    pub padding_x: f32,
    pub padding_y: f32,
    pub icon_size: f32,
    pub font_size: f32,
    pub footer_height: f32,
    pub corner_radius: f32,
    pub dpi_scale: f32,
}

/// Reads `HKCU\...\Themes\Personalize\AppsUseLightTheme`; `0` means dark. A missing value means light,
/// which is the Windows default.
pub fn detect_windows_dark_mode() -> bool {
    let mut value: u32 = 1;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut core::ffi::c_void),
            Some(&mut size),
        )
    };
    status.is_ok() && value == 0
}

pub fn resolve_theme(pref: ThemePreference) -> ThemeColors {
    let dark = match pref {
        ThemePreference::Dark => true,
        ThemePreference::Light => false,
        ThemePreference::Auto => detect_windows_dark_mode(),
    };
    if dark {
        ThemeColors::dark()
    } else {
        ThemeColors::light()
    }
}

struct DensityBase {
    row_height: f32,
    separator_height: f32,
    padding_x: f32,
    padding_y: f32,
    icon_size: f32,
    font_size: f32,
    footer_height: f32,
}

const fn density_base(density: Density) -> DensityBase {
    match density {
        Density::Compact => DensityBase {
            row_height: 22.0,
            separator_height: 7.0,
            padding_x: 8.0,
            padding_y: 3.0,
            icon_size: 16.0,
            font_size: 12.0,
            footer_height: 20.0,
        },
        Density::Classic => DensityBase {
            row_height: 26.0,
            separator_height: 9.0,
            padding_x: 10.0,
            padding_y: 4.0,
            icon_size: 16.0,
            font_size: 12.0,
            footer_height: 22.0,
        },
        Density::Comfortable => DensityBase {
            row_height: 32.0,
            separator_height: 11.0,
            padding_x: 12.0,
            padding_y: 6.0,
            icon_size: 20.0,
            font_size: 13.0,
            footer_height: 26.0,
        },
    }
}

/// Matches the radius DWM uses for `DWMWCP_ROUNDSMALL`, so the drawn outline follows the window clip.
const CORNER_RADIUS_DIP: f32 = 4.0;

pub fn compute_metrics(density: Density, dpi: u32) -> MenuMetrics {
    let dpi = if dpi == 0 { 96 } else { dpi };
    let scale = dpi as f32 / 96.0;
    let base = density_base(density);
    let px = |dip: f32| (dip * scale).round();
    MenuMetrics {
        row_height: px(base.row_height),
        separator_height: px(base.separator_height),
        padding_x: px(base.padding_x),
        padding_y: px(base.padding_y),
        icon_size: px(base.icon_size),
        font_size: base.font_size * scale,
        footer_height: px(base.footer_height),
        corner_radius: CORNER_RADIUS_DIP * scale,
        dpi_scale: scale,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DENSITIES: [Density; 3] = [Density::Compact, Density::Classic, Density::Comfortable];

    fn assert_readable(theme: &ThemeColors) {
        assert!(
            theme.text.contrast_ratio(theme.bg) >= 7.0,
            "label text must meet WCAG AAA on the menu background"
        );
        assert!(
            theme.text.contrast_ratio(theme.hover_bg) >= 7.0,
            "label text must stay AAA-readable on the hover pill"
        );
        assert!(
            theme.shortcut_text.contrast_ratio(theme.bg) >= 4.5,
            "shortcut hints must meet WCAG AA"
        );
        assert!(
            theme.badge_text.contrast_ratio(theme.badge_bg) >= 4.5,
            "timing badge text must meet WCAG AA"
        );
        assert!(
            theme.text_disabled.contrast_ratio(theme.bg) < theme.text.contrast_ratio(theme.bg),
            "disabled text must read dimmer than enabled text"
        );
        assert!(
            theme.text_disabled.contrast_ratio(theme.bg) >= 2.0,
            "disabled text must remain visible"
        );
        assert_ne!(theme.hover_bg, theme.bg, "hover pill must be visible");
        assert_ne!(theme.separator, theme.bg, "separators must be visible");
        assert_ne!(theme.border, theme.bg, "outer border must be visible");
        assert_ne!(
            theme.footer_bg, theme.bg,
            "footer bar must be distinguishable"
        );
    }

    #[test]
    fn light_palette_is_light_and_readable() {
        let theme = ThemeColors::light();
        assert!(!theme.is_dark);
        assert!(theme.bg.relative_luminance() > theme.text.relative_luminance());
        assert_readable(&theme);
    }

    #[test]
    fn dark_palette_is_dark_and_readable() {
        let theme = ThemeColors::dark();
        assert!(theme.is_dark);
        assert!(theme.bg.relative_luminance() < theme.text.relative_luminance());
        assert!(theme.bg.relative_luminance() < ThemeColors::light().bg.relative_luminance());
        assert_readable(&theme);
    }

    #[test]
    fn explicit_preferences_ignore_the_system_setting() {
        assert_eq!(resolve_theme(ThemePreference::Dark), ThemeColors::dark());
        assert_eq!(resolve_theme(ThemePreference::Light), ThemeColors::light());
    }

    #[test]
    fn auto_preference_follows_the_system_setting() {
        let expected = if detect_windows_dark_mode() {
            ThemeColors::dark()
        } else {
            ThemeColors::light()
        };
        assert_eq!(resolve_theme(ThemePreference::Auto), expected);
    }

    #[test]
    fn colorref_uses_gdi_byte_order() {
        assert_eq!(Color::from_hex(0x112233).to_colorref(), 0x0033_2211);
        assert_eq!(
            Color::from_hex(0xFFFFFF).to_unit_rgba(),
            [1.0, 1.0, 1.0, 1.0]
        );
    }

    #[test]
    fn metrics_at_96_dpi_are_whole_unscaled_pixels() {
        for density in DENSITIES {
            let m = compute_metrics(density, 96);
            assert_eq!(m.dpi_scale, 1.0);
            for value in [
                m.row_height,
                m.separator_height,
                m.padding_x,
                m.padding_y,
                m.icon_size,
                m.footer_height,
            ] {
                assert_eq!(
                    value,
                    value.round(),
                    "{density:?} geometry must be pixel aligned"
                );
            }
            assert!(
                m.icon_size + 4.0 <= m.row_height,
                "{density:?} icon must fit inside a row"
            );
            assert!(
                m.font_size * 1.5 <= m.row_height,
                "{density:?} text line must fit inside a row"
            );
            assert!(m.separator_height < m.row_height);
            assert!(m.corner_radius > 0.0);
        }
    }

    #[test]
    fn densities_order_from_compact_to_comfortable() {
        let [compact, classic, comfortable] = DENSITIES.map(|d| compute_metrics(d, 96));
        assert!(compact.row_height < classic.row_height);
        assert!(classic.row_height < comfortable.row_height);
        assert!(compact.padding_y <= classic.padding_y);
        assert!(classic.padding_y <= comfortable.padding_y);
        assert!(compact.footer_height <= comfortable.footer_height);
    }

    #[test]
    fn metrics_scale_with_dpi() {
        for density in DENSITIES {
            let base = compute_metrics(density, 96);
            for dpi in [120u32, 144, 168, 192, 288] {
                let scaled = compute_metrics(density, dpi);
                let factor = dpi as f32 / 96.0;
                assert_eq!(scaled.dpi_scale, factor);
                assert!((scaled.font_size - base.font_size * factor).abs() < 1e-4);
                assert!((scaled.corner_radius - base.corner_radius * factor).abs() < 1e-4);
                for (b, s) in [
                    (base.row_height, scaled.row_height),
                    (base.separator_height, scaled.separator_height),
                    (base.icon_size, scaled.icon_size),
                    (base.footer_height, scaled.footer_height),
                    (base.padding_x, scaled.padding_x),
                ] {
                    assert_eq!(s, s.round(), "{dpi} dpi geometry must be pixel aligned");
                    assert!(
                        (s - b * factor).abs() <= 0.5,
                        "{dpi} dpi: {s} is not {b} x {factor}"
                    );
                }
            }
        }
        let double = compute_metrics(Density::Classic, 192);
        let single = compute_metrics(Density::Classic, 96);
        assert_eq!(double.row_height, single.row_height * 2.0);
        assert_eq!(double.icon_size, single.icon_size * 2.0);
    }

    #[test]
    fn zero_dpi_falls_back_to_96() {
        assert_eq!(
            compute_metrics(Density::Classic, 0),
            compute_metrics(Density::Classic, 96)
        );
    }
}
