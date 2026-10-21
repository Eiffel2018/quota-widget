//! Shared, high-contrast typography for every native viewport and popup.
//!
//! Sizes are logical points. Native display scaling is applied by egui, so the
//! minimum is not multiplied by the screen's pixels-per-point a second time.

use eframe::egui::{self, Color32, FontFamily, FontId, Stroke, TextStyle, Vec2};

pub const REFERENCE_MINIMUM_FONT: f32 = 18.0;
pub const MINIMUM_ROOT_WIDTH: f32 = 430.0;
pub const MAXIMUM_ROOT_WIDTH: f32 = 1000.0;
pub const MAXIMUM_MAIN_SCALE: f32 = 2.5;

/// The floor matches the main widget's 13-point quota-rate label at its current
/// scale, including the same 2.5x cap. A missing/invalid first-frame width uses
/// the smallest supported root window instead of producing invalid geometry.
pub fn minimum_font(root_width: f32) -> f32 {
    let width = if root_width.is_nan() {
        MINIMUM_ROOT_WIDTH
    } else {
        root_width.clamp(MINIMUM_ROOT_WIDTH, MAXIMUM_ROOT_WIDTH)
    };
    REFERENCE_MINIMUM_FONT.max(13.0 * (width / 310.0).min(MAXIMUM_MAIN_SCALE))
}

/// Opaque colors shared by styled controls and explicitly painted text.
///
/// All text colors, including warning/error/link/accent colors, have at least
/// 7:1 contrast against every fill in this palette. `border` is for outlines,
/// never for text. Weak text deliberately uses `text`, too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub text: Color32,
    pub background: Color32,
    pub panel: Color32,
    pub widget: Color32,
    pub hovered: Color32,
    pub active: Color32,
    pub border: Color32,
    pub accent: Color32,
    pub warning: Color32,
    pub error: Color32,
    pub link: Color32,
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            text: Color32::from_rgb(220, 247, 229),
            background: Color32::from_rgb(23, 39, 34),
            panel: Color32::from_rgb(28, 47, 40),
            widget: Color32::from_rgb(36, 58, 48),
            hovered: Color32::from_rgb(40, 64, 52),
            active: Color32::from_rgb(44, 70, 57),
            border: Color32::from_rgb(138, 178, 155),
            accent: Color32::from_rgb(171, 228, 188),
            warning: Color32::from_rgb(255, 218, 128),
            error: Color32::from_rgb(255, 211, 206),
            link: Color32::from_rgb(183, 224, 255),
        }
    } else {
        Palette {
            text: Color32::from_rgb(29, 61, 45),
            background: Color32::from_rgb(240, 248, 240),
            panel: Color32::from_rgb(247, 252, 247),
            widget: Color32::from_rgb(232, 242, 232),
            hovered: Color32::from_rgb(219, 235, 221),
            active: Color32::from_rgb(207, 227, 210),
            border: Color32::from_rgb(81, 116, 93),
            accent: Color32::from_rgb(31, 77, 45),
            warning: Color32::from_rgb(95, 55, 0),
            error: Color32::from_rgb(126, 20, 35),
            link: Color32::from_rgb(16, 66, 103),
        }
    }
}

/// Configure standard text and controls without flattening explicit RichText
/// sizes or semantic colors. Call for each viewport using the root window's
/// `minimum_font`, so detached windows and tooltips share the same readable floor.
///
/// Standard styles and spacing are assigned from the supplied size, rather than
/// multiplied by their previous values. Reapplying this function is idempotent
/// and shrinking a window restores the appropriate size without cumulative zoom.
pub fn configure_style(style: &mut egui::Style, dark: bool, font_size: f32) {
    let font_size = if font_size.is_finite() {
        font_size.max(REFERENCE_MINIMUM_FONT)
    } else {
        REFERENCE_MINIMUM_FONT
    };
    let scale = font_size / REFERENCE_MINIMUM_FONT;

    for (text_style, family, size) in [
        (TextStyle::Body, FontFamily::Proportional, font_size),
        (TextStyle::Button, FontFamily::Proportional, font_size),
        (TextStyle::Small, FontFamily::Proportional, font_size),
        (TextStyle::Monospace, FontFamily::Monospace, font_size),
        (
            TextStyle::Heading,
            FontFamily::Proportional,
            font_size * 1.25,
        ),
    ] {
        style
            .text_styles
            .entry(text_style)
            .and_modify(|font| font.size = size)
            .or_insert_with(|| FontId::new(size, family));
    }
    // Preserve named styles and custom font families while enforcing the floor.
    for font in style.text_styles.values_mut() {
        font.size = font.size.max(font_size);
    }
    if let Some(font) = style.override_font_id.as_mut() {
        font.size = font.size.max(font_size);
    }

    let spacing = &mut style.spacing;
    spacing.item_spacing = Vec2::new(10.0, 8.0) * scale;
    spacing.button_padding = Vec2::new(10.0, 6.0) * scale;
    spacing.interact_size = Vec2::new(48.0, 32.0) * scale;
    spacing.window_margin = egui::Margin::same((12.0 * scale).round() as i8);
    spacing.menu_margin = egui::Margin::same((8.0 * scale).round() as i8);
    spacing.indent = 24.0 * scale;
    spacing.icon_width = 22.0 * scale;
    spacing.icon_width_inner = 14.0 * scale;
    spacing.icon_spacing = 8.0 * scale;
    spacing.slider_width = 180.0 * scale;
    spacing.slider_rail_height = 8.0 * scale;
    spacing.combo_width = 180.0 * scale;
    spacing.text_edit_width = 280.0 * scale;
    spacing.tooltip_width = ((MINIMUM_ROOT_WIDTH - 40.0) * scale).min(680.0);
    spacing.menu_width = (400.0 * scale).min(800.0);
    spacing.combo_height = 320.0 * scale;
    spacing.scroll.bar_width = 14.0 * scale;
    spacing.scroll.handle_min_length = 32.0 * scale;

    let colors = palette(dark);
    let mut visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    visuals.override_text_color = Some(colors.text);
    visuals.weak_text_color = Some(colors.text);
    visuals.weak_text_alpha = 1.0;
    // egui otherwise halves the opacity of *all* disabled-widget paint,
    // including text. Interaction still remains disabled with this set to 1.
    visuals.disabled_alpha = 1.0;
    visuals.panel_fill = colors.background;
    visuals.window_fill = colors.panel;
    visuals.window_stroke = Stroke::new(1.0_f32, colors.border);
    visuals.faint_bg_color = colors.widget;
    visuals.extreme_bg_color = colors.background;
    visuals.text_edit_bg_color = Some(colors.background);
    visuals.code_bg_color = colors.widget;
    visuals.hyperlink_color = colors.link;
    visuals.warn_fg_color = colors.warning;
    visuals.error_fg_color = colors.error;
    visuals.selection.bg_fill = colors.active;
    visuals.selection.stroke = Stroke::new(1.5_f32, colors.text);
    visuals.text_cursor.stroke = Stroke::new(2.0_f32, colors.text);

    for (widget, fill, border_width) in [
        (&mut visuals.widgets.noninteractive, colors.panel, 1.0),
        (&mut visuals.widgets.inactive, colors.widget, 1.0_f32),
        (&mut visuals.widgets.hovered, colors.hovered, 1.5),
        (&mut visuals.widgets.active, colors.active, 2.0),
        (&mut visuals.widgets.open, colors.active, 2.0),
    ] {
        widget.bg_fill = fill;
        widget.weak_bg_fill = fill;
        widget.bg_stroke = Stroke::new(border_width, colors.border);
        widget.fg_stroke = Stroke::new(1.5_f32, colors.text);
    }
    style.visuals = visuals;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(color: Color32) -> f64 {
        let linear = |channel: u8| {
            let channel = f64::from(channel) / 255.0;
            if channel <= 0.04045 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(color.r()) + 0.7152 * linear(color.g()) + 0.0722 * linear(color.b())
    }

    fn contrast(foreground: Color32, background: Color32) -> f64 {
        assert_eq!(foreground.a(), 255, "text must remain opaque");
        assert_eq!(background.a(), 255, "contrast test needs an opaque fill");
        let foreground = luminance(foreground);
        let background = luminance(background);
        (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05)
    }

    fn assert_contrast(foreground: Color32, background: Color32, minimum: f64) {
        let ratio = contrast(foreground, background);
        assert!(
            ratio >= minimum,
            "{foreground:?} against {background:?} has contrast {ratio:.3}:1; expected {minimum}:1"
        );
    }

    #[test]
    fn minimum_font_matches_quota_rate_and_caps_with_main_widget() {
        for width in [0.0_f32, 310.0, 430.0, 600.0, 775.0, 1000.0, 2000.0] {
            let rate_size = 13.0 * (width.clamp(430.0, 1000.0) / 310.0).min(2.5);
            assert_eq!(minimum_font(width), 18.0_f32.max(rate_size));
        }
        assert_eq!(minimum_font(430.0), 13.0 * (430.0 / 310.0));
        assert_eq!(minimum_font(1000.0), 32.5);
        assert_eq!(minimum_font(f32::NAN), minimum_font(430.0));
        assert_eq!(minimum_font(f32::NEG_INFINITY), minimum_font(430.0));
        assert_eq!(minimum_font(f32::INFINITY), 32.5);
    }

    #[test]
    fn every_standard_text_style_meets_floor_in_both_modes() {
        for dark in [false, true] {
            for width in [430.0, 600.0, 1000.0] {
                let minimum = minimum_font(width);
                let mut style = egui::Style::default();
                configure_style(&mut style, dark, minimum);
                for text_style in [
                    TextStyle::Body,
                    TextStyle::Button,
                    TextStyle::Small,
                    TextStyle::Monospace,
                ] {
                    assert_eq!(style.text_styles[&text_style].size, minimum);
                }
                assert!(style.text_styles[&TextStyle::Heading].size >= minimum * 1.25);
                assert_eq!(
                    style.text_styles[&TextStyle::Monospace].family,
                    FontFamily::Monospace
                );
                assert!(style.spacing.interact_size.y >= minimum + 12.0);
                assert!(style.spacing.tooltip_width >= 20.0 * minimum);
                assert!(style.spacing.tooltip_width <= width - 20.0);
            }
        }
    }

    #[test]
    fn reapplying_style_never_compounds_sizes_and_can_shrink_again() {
        let mut style = egui::Style::default();
        configure_style(&mut style, true, minimum_font(1000.0));
        configure_style(&mut style, false, minimum_font(430.0));
        let once = style.clone();
        configure_style(&mut style, false, minimum_font(430.0));
        assert_eq!(style.text_styles, once.text_styles);
        assert_eq!(style.spacing, once.spacing);
        assert_eq!(style.visuals, once.visuals);
        assert_eq!(
            style.text_styles[&TextStyle::Body].size,
            minimum_font(430.0)
        );
        for invalid in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            configure_style(&mut style, true, invalid);
            assert_eq!(
                style.text_styles[&TextStyle::Body].size,
                REFERENCE_MINIMUM_FONT
            );
        }
    }

    #[test]
    fn regular_weak_strong_and_disabled_text_share_readable_foreground() {
        for dark in [false, true] {
            let mut style = egui::Style::default();
            configure_style(&mut style, dark, 18.0);
            let visuals = &style.visuals;
            let foreground = palette(dark).text;
            assert_eq!(visuals.text_color(), foreground);
            assert_eq!(visuals.weak_text_color(), foreground);
            assert_eq!(visuals.strong_text_color(), foreground);
            assert_eq!(visuals.disable(foreground), foreground);
            for background in [
                visuals.window_fill,
                visuals.panel_fill,
                visuals.faint_bg_color,
                visuals.extreme_bg_color,
                visuals.code_bg_color,
                visuals.text_edit_bg_color(),
                visuals.selection.bg_fill,
                egui::Frame::popup(&style).fill,
                egui::Frame::window(&style).fill,
            ] {
                assert_contrast(foreground, background, 7.0);
                assert_contrast(visuals.disable(foreground), background, 7.0);
            }
        }
    }

    #[test]
    fn every_widget_state_has_aaa_text_and_visible_control_outlines() {
        for dark in [false, true] {
            let mut style = egui::Style::default();
            configure_style(&mut style, dark, 18.0);
            let widgets = &style.visuals.widgets;
            for state in [
                widgets.noninteractive,
                widgets.inactive,
                widgets.hovered,
                widgets.active,
                widgets.open,
            ] {
                for background in [state.bg_fill, state.weak_bg_fill] {
                    assert_contrast(state.text_color(), background, 7.0);
                    assert_contrast(style.visuals.disable(state.text_color()), background, 7.0);
                    assert_contrast(state.bg_stroke.color, background, 3.0);
                }
            }
        }
    }

    #[test]
    fn semantic_text_colors_have_aaa_contrast_on_every_palette_fill() {
        for dark in [false, true] {
            let colors = palette(dark);
            for foreground in [
                colors.text,
                colors.warning,
                colors.error,
                colors.link,
                colors.accent,
            ] {
                for background in [
                    colors.background,
                    colors.panel,
                    colors.widget,
                    colors.hovered,
                    colors.active,
                ] {
                    assert_contrast(foreground, background, 7.0);
                }
            }
        }
    }

    #[test]
    fn explicit_rich_text_color_and_larger_type_are_preserved() {
        for dark in [false, true] {
            let mut style = egui::Style::default();
            configure_style(&mut style, dark, 24.0);
            let color = palette(dark).warning;
            let text: egui::WidgetText = egui::RichText::new("重要 1.30 百分點")
                .size(42.0)
                .color(color)
                .into();
            let job =
                text.into_layout_job(&style, egui::FontSelection::Default, egui::Align::Center);
            assert_eq!(job.sections[0].format.font_id.size, 42.0);
            assert_eq!(job.sections[0].format.color, color);
        }
    }
}
