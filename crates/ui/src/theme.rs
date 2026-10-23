//! Theme tokens — every visual decision flows through `Theme` so skins are a
//! data swap (JSON config), not a code change. Named values resolve to live
//! `ThemeBrush` resources (follow Windows light/dark); "#rrggbb"/"#aarrggbb"
//! resolve to solid colors for full custom skins.

use serde::{Deserialize, Serialize};
use windows_canvas::ColorF;
use windows_reactor::{Brush, Color, ThemeBrush, Thickness};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    /// Named ("accent","card","stroke","text","subtle","danger","solid") or #hex.
    pub accent: Option<String>,
    pub accent_soft: Option<String>,
    pub text: Option<String>,
    pub subtle: Option<String>,
    pub danger: Option<String>,
    pub warn: Option<String>,
    pub ok: Option<String>,
    pub card_bg: Option<String>,
    pub card_border: Option<String>,
    pub divider: Option<String>,
    pub page_bg: Option<String>,
    pub radius: Option<f64>,
    pub card_pad: Option<f64>,
    pub gap: Option<f64>,
    pub section_gap: Option<f64>,
    pub line_separators: Option<bool>,
    /// Thin accent strip on the left edge of cards.
    pub accent_edge: Option<bool>,
    /// Honored by Direct2D chart text (DirectWrite); XAML controls follow the
    /// system font until reactor exposes a family setter.
    pub font_family: Option<String>,
    pub title_size: Option<f64>,
    pub h2_size: Option<f64>,
    pub body_size: Option<f64>,
    pub label_size: Option<f64>,
}

/// Resolved render-time tokens.
#[derive(Clone, Debug)]
pub struct Theme {
    pub accent: Brush,
    pub accent_soft: Brush,
    pub text: Brush,
    pub subtle: Brush,
    pub danger: Brush,
    pub warn: Brush,
    pub ok: Brush,
    pub card_bg: Brush,
    pub card_border: Brush,
    pub divider: Brush,
    pub page_bg: Option<Brush>,
    /// Opaque page color for canvas regions that must occlude other drawing.
    pub page_cf: ColorF,
    /// Direct2D copies of key colors — hex config values map exactly; named
    /// theme brushes fall back to Fluent constants below (theme brushes have
    /// no RGB at this layer).
    pub accent_cf: ColorF,
    pub subtle_cf: ColorF,
    pub divider_cf: ColorF,
    /// Activity heatmap: empty, then four opaque tones from low to high.
    pub heat_cf: [ColorF; 5],
    /// Card fill as drawn by the canvas: SwapChainPanel pixels composite
    /// straight onto the window surface, so transparency can't "see" the
    /// card's translucent ThemeBrush beneath — the canvas paints the same
    /// translucent fill itself and lands on the identical result.
    pub card_cf: ColorF,
    /// DirectWrite family for chart text.
    pub font_family: String,
    pub radius: f64,
    pub pad: f64,
    pub gap: f64,
    pub section_gap: f64,
    pub line_separators: bool,
    pub accent_edge: bool,
    pub title_size: f64,
    pub h2_size: f64,
    pub body_size: f64,
    pub label_size: f64,
}

fn brush_of(s: Option<&str>, default: Brush) -> Brush {
    let Some(s) = s else {
        return default;
    };
    match s.trim().to_ascii_lowercase().as_str() {
        "accent" => Brush::Theme(ThemeBrush::Accent),
        "accent_text" | "accenttext" => Brush::Theme(ThemeBrush::AccentText),
        "text" | "primary" => Brush::Theme(ThemeBrush::PrimaryText),
        "subtle" => Brush::Theme(ThemeBrush::AccentText),
        "card" | "card_bg" => Brush::Theme(ThemeBrush::CardBackground),
        "stroke" | "card_stroke" => Brush::Theme(ThemeBrush::CardStroke),
        "danger" | "critical" => Brush::Theme(ThemeBrush::SystemCritical),
        "danger_bg" | "critical_bg" => Brush::Theme(ThemeBrush::SystemCriticalBackground),
        "solid" | "background" => Brush::Theme(ThemeBrush::SolidBackground),
        hex => parse_hex(hex).map(Brush::Solid).unwrap_or(default),
    }
}

/// Canvas colors: hex strings map 1:1; named theme brushes can't be read back
/// as RGB, so they resolve to the Fluent defaults below.
fn colorf_of(s: Option<&str>, default: ColorF) -> ColorF {
    let Some(s) = s else { return default };
    let s = s.trim();
    let h = s.strip_prefix('#').unwrap_or(s);
    u32::from_str_radix(h, 16)
        .ok()
        .map(|v| match h.len() {
            6 => ColorF::from_rgb8(
                ((v >> 16) & 0xff) as u8,
                ((v >> 8) & 0xff) as u8,
                (v & 0xff) as u8,
            ),
            8 => ColorF::from_rgba8(
                ((v >> 16) & 0xff) as u8,
                ((v >> 8) & 0xff) as u8,
                (v & 0xff) as u8,
                ((v >> 24) & 0xff) as u8,
            ),
            _ => default,
        })
        .unwrap_or(default)
}

/// Keep the accent's hue/saturation while spreading the four levels across
/// lightness. Alpha-only steps blend into grey and compress the visible range.
fn heat_palette(accent: ColorF, divider: ColorF, light: bool) -> [ColorF; 5] {
    let min = accent.r.min(accent.g).min(accent.b);
    let max = accent.r.max(accent.g).max(accent.b);
    let chroma = max - min;
    let lightness = f32::midpoint(max, min);
    let saturation = if chroma > 0.0 {
        chroma / (1.0 - (2.0 * lightness - 1.0).abs())
    } else {
        0.0
    };
    let tones = if light {
        [0.86, 0.66, 0.46, 0.26]
    } else {
        [0.32, 0.50, 0.70, 0.88]
    };
    let mut colors = [ColorF {
        a: divider.a * 0.5,
        ..divider
    }; 5];
    for (color, tone) in colors[1..].iter_mut().zip(tones) {
        let span = (1.0 - (2.0 * tone - 1.0_f32).abs()) * saturation;
        let channel = |c: f32| {
            tone - span * 0.5
                + if chroma > 0.0 {
                    (c - min) / chroma * span
                } else {
                    0.0
                }
        };
        *color = ColorF::rgb(channel(accent.r), channel(accent.g), channel(accent.b));
    }
    colors
}

fn parse_hex(s: &str) -> Option<Color> {
    let h = s.strip_prefix('#').unwrap_or(s);
    let v = u32::from_str_radix(h, 16).ok()?;
    match h.len() {
        6 => Some(Color::rgb(
            ((v >> 16) & 0xff) as u8,
            ((v >> 8) & 0xff) as u8,
            (v & 0xff) as u8,
        )),
        8 => Some(Color::argb(
            ((v >> 24) & 0xff) as u8,
            ((v >> 16) & 0xff) as u8,
            ((v >> 8) & 0xff) as u8,
            (v & 0xff) as u8,
        )),
        _ => None,
    }
}

/// Whether the window renders light: an explicit mode wins; "system" (or
/// unset) follows the apps theme in the registry.
pub fn is_light(window_theme: &str) -> bool {
    match window_theme {
        "light" => true,
        "dark" => false,
        _ => system_apps_light(),
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn system_apps_light() -> bool {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    let key: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"
        .encode_utf16()
        .chain([0])
        .collect();
    let name: Vec<u16> = "AppsUseLightTheme".encode_utf16().chain([0]).collect();
    let mut data = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&raw mut data).cast(),
            &raw mut size,
        )
    };
    status == 0 && data == 1
}

#[cfg(not(windows))]
fn system_apps_light() -> bool {
    false
}

impl Theme {
    /// `light`: the canvas colors below can't be read back from the named
    /// theme brushes, so their defaults come in a dark and a light set (see
    /// `is_light`) — without it the charts kept dark-skin constants on a
    /// white card (grey chart boxes, a pale accent that disagreed with the
    /// legend swatches).
    pub fn resolve(cfg: &ThemeConfig, light: bool) -> Self {
        // Win11 accent — dark #76B9ED, light #0067C0; hex overrides map exactly.
        let accent_cf = colorf_of(
            cfg.accent.as_deref(),
            if light {
                ColorF::from_rgb8(0x00, 0x67, 0xC0)
            } else {
                ColorF::from_rgb8(0x76, 0xB9, 0xED)
            },
        );
        let divider_cf = colorf_of(
            cfg.divider.as_deref(),
            ColorF::from_rgba8(0x80, 0x80, 0x80, 0x44),
        );
        Self {
            accent: brush_of(cfg.accent.as_deref(), Brush::Theme(ThemeBrush::Accent)),
            accent_soft: brush_of(
                cfg.accent_soft.as_deref(),
                Brush::Theme(ThemeBrush::AccentText),
            ),
            text: brush_of(cfg.text.as_deref(), Brush::Theme(ThemeBrush::PrimaryText)),
            subtle: brush_of(cfg.subtle.as_deref(), Brush::Theme(ThemeBrush::AccentText)),
            danger: brush_of(
                cfg.danger.as_deref(),
                Brush::Theme(ThemeBrush::SystemCritical),
            ),
            // Fluent warning yellow — not among the 8 theme brushes.
            warn: brush_of(cfg.warn.as_deref(), Brush::Solid(Color::rgb(255, 185, 0))),
            // Fluent success green.
            ok: brush_of(cfg.ok.as_deref(), Brush::Solid(Color::rgb(16, 168, 116))),
            card_bg: brush_of(
                cfg.card_bg.as_deref(),
                Brush::Theme(ThemeBrush::CardBackground),
            ),
            card_border: brush_of(
                cfg.card_border.as_deref(),
                Brush::Theme(ThemeBrush::CardStroke),
            ),
            divider: brush_of(cfg.divider.as_deref(), Brush::Theme(ThemeBrush::CardStroke)),
            page_bg: cfg
                .page_bg
                .as_deref()
                .map(|s| brush_of(Some(s), Brush::Theme(ThemeBrush::SolidBackground))),
            page_cf: colorf_of(
                cfg.page_bg.as_deref(),
                if light {
                    ColorF::from_rgba8(0xF3, 0xF3, 0xF3, 0xFF)
                } else {
                    ColorF::from_rgba8(0x20, 0x20, 0x20, 0xFF)
                },
            ),
            accent_cf,
            subtle_cf: colorf_of(
                cfg.subtle.as_deref(),
                if light {
                    ColorF::from_rgba8(0x6B, 0x6B, 0x6B, 0xFF)
                } else {
                    ColorF::from_rgba8(0x9E, 0x9E, 0x9E, 0xFF)
                },
            ),
            divider_cf,
            heat_cf: heat_palette(accent_cf, divider_cf, light),
            // Fluent CardBackgroundFillColorDefault — white @ ~5% (dark) /
            // @ ~70% (light): the canvas paints this over the same page the
            // card brush blends onto, landing pixel-identical. Hex card_bg
            // overrides map 1:1.
            card_cf: colorf_of(
                cfg.card_bg.as_deref(),
                ColorF::from_rgba8(0xFF, 0xFF, 0xFF, if light { 0xB3 } else { 0x0D }),
            ),
            font_family: cfg.font_family.clone().unwrap_or_else(|| "Segoe UI".into()),
            radius: cfg.radius.unwrap_or(8.0),
            pad: cfg.card_pad.unwrap_or(16.0),
            gap: cfg.gap.unwrap_or(12.0),
            section_gap: cfg.section_gap.unwrap_or(14.0),
            line_separators: cfg.line_separators.unwrap_or(true),
            accent_edge: cfg.accent_edge.unwrap_or(false),
            title_size: cfg.title_size.unwrap_or(22.0),
            h2_size: cfg.h2_size.unwrap_or(14.0),
            body_size: cfg.body_size.unwrap_or(12.0),
            label_size: cfg.label_size.unwrap_or(11.0),
        }
    }

    /// Card border thickness — side-aware so `accent_edge` only paints left.
    pub fn card_border_thickness(&self) -> Thickness {
        if self.accent_edge {
            Thickness::new(2.0, 1.0, 1.0, 1.0)
        } else {
            Thickness::uniform(1.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(c: ColorF) -> f32 {
        let linear = |v: f32| {
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(c.r) + 0.7152 * linear(c.g) + 0.0722 * linear(c.b)
    }

    #[test]
    fn heat_levels_have_separated_luminance_in_both_modes() {
        for light in [false, true] {
            let theme = Theme::resolve(&ThemeConfig::default(), light);
            assert_eq!(theme.heat_cf[0].a, theme.divider_cf.a * 0.5);
            for pair in theme.heat_cf[1..].windows(2) {
                let delta = luminance(pair[1]) - luminance(pair[0]);
                assert!(if light { delta < -0.1 } else { delta > 0.1 });
            }
        }
    }

    #[test]
    fn heat_palette_preserves_custom_hue_and_handles_greys_and_alpha() {
        for accent in ["#4028B478", "#000000", "#FFFFFF", "#808080"] {
            let cfg = ThemeConfig {
                accent: Some(accent.into()),
                ..Default::default()
            };
            for light in [false, true] {
                let theme = Theme::resolve(&cfg, light);
                let colors = &theme.heat_cf[1..];
                for &c in colors {
                    assert_eq!(c.a, 1.0);
                    assert!(
                        [c.r, c.g, c.b]
                            .iter()
                            .all(|v| v.is_finite() && (0.0..=1.0).contains(v))
                    );
                    if accent == "#4028B478" {
                        assert!(c.g > c.b && c.b > c.r);
                    } else {
                        assert_eq!(c.r, c.g);
                        assert_eq!(c.g, c.b);
                    }
                }
                for pair in colors.windows(2) {
                    let delta = luminance(pair[1]) - luminance(pair[0]);
                    assert!(if light { delta < 0.0 } else { delta > 0.0 });
                }
            }
        }
    }
}
