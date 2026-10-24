use crate::files::{atomic_write, ensure_safe_path};
use anyhow::{Context, Result, ensure};
use eframe::egui::{self, Color32, FontFamily, FontId, Stroke, Vec2};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, io::Read, path::PathBuf, sync::Arc};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

impl Appearance {
    #[cfg(debug_assertions)]
    pub(super) fn parse(value: &str) -> Result<Self> {
        match value {
            "system" => Ok(Self::System),
            "light" => Ok(Self::Light),
            "dark" => Ok(Self::Dark),
            _ => anyhow::bail!("Appearance must be system, light, or dark"),
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::System => "Use Windows setting",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    pub(super) fn apply(self, context: &egui::Context) {
        let (preference, native) = match self {
            Self::System => (
                egui::ThemePreference::System,
                egui::SystemTheme::SystemDefault,
            ),
            Self::Light => (egui::ThemePreference::Light, egui::SystemTheme::Light),
            Self::Dark => (egui::ThemePreference::Dark, egui::SystemTheme::Dark),
        };
        context.set_theme(preference);
        context.send_viewport_cmd(egui::ViewportCommand::SetTheme(native));
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Preferences {
    version: u8,
    appearance: Appearance,
}

pub(super) struct AppearanceStore {
    path: Option<PathBuf>,
    pub(super) appearance: Appearance,
}

impl AppearanceStore {
    pub(super) fn load(root: &std::path::Path) -> Result<Self> {
        let path = root.join("interface.json");
        ensure_safe_path(&path)?;
        let appearance = if path.try_exists()? {
            ensure!(
                path.metadata()?.len() <= 4096,
                "Interface preferences exceed their size limit"
            );
            let mut bytes = Vec::new();
            fs::File::open(&path)?.take(4097).read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() <= 4096,
                "Interface preferences grew beyond their size limit"
            );
            let preferences: Preferences =
                serde_json::from_slice(&bytes).context("Cannot read interface preferences")?;
            ensure!(
                preferences.version == 1,
                "Unsupported interface preferences version"
            );
            preferences.appearance
        } else {
            Appearance::System
        };
        Ok(Self {
            path: Some(path),
            appearance,
        })
    }

    #[cfg(debug_assertions)]
    pub(super) fn preview(appearance: Appearance) -> Self {
        Self {
            path: None,
            appearance,
        }
    }

    pub(super) fn set(&mut self, appearance: Appearance) -> Result<()> {
        if let Some(path) = &self.path {
            atomic_write(
                path,
                &serde_json::to_vec_pretty(&Preferences {
                    version: 1,
                    appearance,
                })?,
            )
            .context("Cannot save interface appearance")?;
        }
        self.appearance = appearance;
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(super) struct Palette {
    pub(super) canvas: Color32,
    pub(super) surface: Color32,
    pub(super) alternate: Color32,
    pub(super) border: Color32,
    pub(super) control_border: Color32,
    pub(super) text: Color32,
    pub(super) secondary: Color32,
    pub(super) accent: Color32,
    pub(super) on_accent: Color32,
    pub(super) selection: Color32,
    pub(super) success: Color32,
    pub(super) warning: Color32,
    pub(super) error: Color32,
    pub(super) info_background: Color32,
    pub(super) warning_background: Color32,
    pub(super) error_background: Color32,
    pub(super) disabled: Color32,
}

impl Palette {
    pub(super) fn new(dark: bool) -> Self {
        if dark {
            Self {
                canvas: Color32::from_rgb(32, 32, 32),
                surface: Color32::from_rgb(43, 43, 43),
                alternate: Color32::from_rgb(49, 49, 49),
                border: Color32::from_rgb(68, 68, 68),
                control_border: Color32::from_rgb(125, 125, 125),
                text: Color32::from_rgb(245, 245, 245),
                secondary: Color32::from_rgb(190, 190, 190),
                accent: Color32::from_rgb(96, 205, 255),
                on_accent: Color32::from_rgb(0, 36, 56),
                selection: Color32::from_rgb(43, 63, 78),
                success: Color32::from_rgb(108, 203, 95),
                warning: Color32::from_rgb(255, 214, 111),
                error: Color32::from_rgb(255, 153, 164),
                info_background: Color32::from_rgb(30, 51, 66),
                warning_background: Color32::from_rgb(67, 55, 29),
                error_background: Color32::from_rgb(67, 37, 41),
                disabled: Color32::from_rgb(55, 55, 55),
            }
        } else {
            Self {
                canvas: Color32::from_rgb(243, 243, 243),
                surface: Color32::WHITE,
                alternate: Color32::from_rgb(248, 248, 248),
                border: Color32::from_rgb(217, 217, 217),
                control_border: Color32::from_rgb(138, 138, 138),
                text: Color32::from_rgb(26, 26, 26),
                secondary: Color32::from_rgb(82, 82, 82),
                accent: Color32::from_rgb(0, 95, 184),
                on_accent: Color32::WHITE,
                selection: Color32::from_rgb(229, 239, 250),
                success: Color32::from_rgb(15, 108, 34),
                warning: Color32::from_rgb(135, 78, 0),
                error: Color32::from_rgb(181, 34, 41),
                info_background: Color32::from_rgb(234, 243, 252),
                warning_background: Color32::from_rgb(255, 244, 206),
                error_background: Color32::from_rgb(253, 235, 237),
                disabled: Color32::from_rgb(235, 235, 235),
            }
        }
    }

    pub(super) fn of(ui: &egui::Ui) -> Self {
        Self::new(ui.visuals().dark_mode)
    }
}

pub(super) fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("Windows semibold".into()))
}

pub(super) fn symbol(text: &str, size: f32) -> egui::RichText {
    egui::RichText::new(text).font(FontId::new(size, FontFamily::Name("Windows icons".into())))
}

pub(super) fn install(context: &egui::Context) -> Result<()> {
    let windows = std::env::var_os("SystemRoot").context("SystemRoot is unavailable")?;
    let directory = PathBuf::from(windows).join("Fonts");
    let mut fonts = egui::FontDefinitions::default();
    for (name, file) in [
        ("Windows UI", "segoeui.ttf"),
        ("Windows semibold", "seguisb.ttf"),
        ("Windows icons", "segmdl2.ttf"),
        ("Windows mono", "consola.ttf"),
    ] {
        let data = fs::read(directory.join(file))
            .with_context(|| format!("Cannot load the installed Windows font {file}"))?;
        fonts
            .font_data
            .insert(name.into(), Arc::new(egui::FontData::from_owned(data)));
    }
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "Windows UI".into());
    fonts
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .insert(0, "Windows mono".into());
    fonts.families.insert(
        FontFamily::Name("Windows semibold".into()),
        vec!["Windows semibold".into(), "Windows UI".into()],
    );
    fonts.families.insert(
        FontFamily::Name("Windows icons".into()),
        vec!["Windows icons".into()],
    );
    context.set_fonts(fonts);
    install_styles(context);
    Ok(())
}

fn install_styles(context: &egui::Context) {
    for theme in [egui::Theme::Light, egui::Theme::Dark] {
        let colors = Palette::new(theme == egui::Theme::Dark);
        let mut style = (*context.style_of(theme)).clone();
        let mut visuals = if theme == egui::Theme::Dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        visuals.override_text_color = None;
        visuals.weak_text_color = Some(colors.secondary);
        visuals.panel_fill = colors.canvas;
        visuals.window_fill = colors.surface;
        visuals.extreme_bg_color = colors.surface;
        visuals.text_edit_bg_color = Some(colors.surface);
        visuals.faint_bg_color = colors.alternate;
        visuals.code_bg_color = colors.alternate;
        visuals.hyperlink_color = colors.accent;
        visuals.warn_fg_color = colors.warning;
        visuals.error_fg_color = colors.error;
        visuals.selection.bg_fill = colors.selection;
        visuals.selection.stroke = Stroke::new(1.0, colors.accent);
        visuals.window_corner_radius = egui::CornerRadius::same(6);
        visuals.menu_corner_radius = egui::CornerRadius::same(6);
        visuals.window_stroke = Stroke::new(1.0, colors.border);
        visuals.window_shadow = egui::epaint::Shadow::NONE;
        visuals.button_frame = true;
        visuals.interact_cursor = None;
        visuals.disabled_alpha = 0.7;
        for widget in [
            &mut visuals.widgets.noninteractive,
            &mut visuals.widgets.inactive,
            &mut visuals.widgets.hovered,
            &mut visuals.widgets.active,
            &mut visuals.widgets.open,
        ] {
            widget.corner_radius = egui::CornerRadius::same(4);
            widget.fg_stroke = Stroke::new(1.0, colors.text);
            widget.bg_stroke = Stroke::new(1.0, colors.control_border);
            widget.expansion = 0.0;
        }
        visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, colors.border);
        visuals.widgets.noninteractive.bg_fill = colors.disabled;
        visuals.widgets.inactive.bg_fill = colors.surface;
        visuals.widgets.inactive.weak_bg_fill = colors.surface;
        visuals.widgets.hovered.bg_fill = colors.alternate;
        visuals.widgets.hovered.weak_bg_fill = colors.alternate;
        visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, colors.accent);
        visuals.widgets.active.bg_fill = colors.selection;
        visuals.widgets.active.weak_bg_fill = colors.selection;
        visuals.widgets.active.bg_stroke = Stroke::new(1.5, colors.accent);
        visuals.widgets.open.bg_fill = colors.selection;
        visuals.widgets.open.weak_bg_fill = colors.selection;
        visuals.text_cursor.stroke = Stroke::new(1.5, colors.accent);
        style.visuals = visuals;
        style.spacing.item_spacing = Vec2::new(10.0, 6.0);
        style.spacing.button_padding = Vec2::new(14.0, 7.0);
        style.spacing.interact_size = Vec2::new(36.0, 32.0);
        style.spacing.icon_width = 18.0;
        style.spacing.icon_width_inner = 12.0;
        style.spacing.indent = 20.0;
        style.spacing.scroll = egui::style::ScrollStyle::solid();
        style.text_styles = BTreeMap::from([
            (egui::TextStyle::Heading, semibold(24.0)),
            (egui::TextStyle::Body, FontId::proportional(14.0)),
            (egui::TextStyle::Button, FontId::proportional(14.0)),
            (egui::TextStyle::Small, FontId::proportional(12.0)),
            (egui::TextStyle::Monospace, FontId::monospace(12.0)),
        ]);
        context.set_style_of(theme, style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(color: Color32) -> f64 {
        let linear = |byte| {
            let value = f64::from(byte) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(color.r()) + 0.7152 * linear(color.g()) + 0.0722 * linear(color.b())
    }

    fn contrast(first: Color32, second: Color32) -> f64 {
        let first = luminance(first);
        let second = luminance(second);
        (first.max(second) + 0.05) / (first.min(second) + 0.05)
    }

    #[test]
    fn both_palettes_keep_normal_text_and_statuses_above_aa_contrast() {
        for dark in [false, true] {
            let p = Palette::new(dark);
            for background in [p.canvas, p.surface, p.alternate] {
                for foreground in [p.text, p.secondary, p.accent, p.success, p.warning, p.error] {
                    assert!(
                        contrast(foreground, background) >= 4.5,
                        "dark={dark} {foreground:?} on {background:?}"
                    );
                }
            }
            for (foreground, background) in [
                (p.on_accent, p.accent),
                (p.accent, p.info_background),
                (p.warning, p.warning_background),
                (p.error, p.error_background),
                (p.accent, p.selection),
                (p.secondary, p.disabled),
            ] {
                assert!(contrast(foreground, background) >= 4.5);
            }
        }
    }

    #[test]
    fn style_switches_keep_light_and_dark_surfaces_and_fonts_consistent() {
        let context = egui::Context::default();
        install_styles(&context);
        for theme in [egui::Theme::Light, egui::Theme::Dark] {
            context.set_theme(theme);
            let p = Palette::new(theme == egui::Theme::Dark);
            assert_eq!(context.style().visuals.window_fill, p.surface);
            assert_eq!(context.style().visuals.text_color(), p.text);
            assert_eq!(
                context.style().text_styles[&egui::TextStyle::Body].size,
                14.0
            );
        }
    }

    #[test]
    fn system_theme_changes_use_the_correct_complete_palette() {
        let context = egui::Context::default();
        install_styles(&context);
        Appearance::System.apply(&context);
        for theme in [egui::Theme::Light, egui::Theme::Dark, egui::Theme::Light] {
            let _ = context.run(
                egui::RawInput {
                    system_theme: Some(theme),
                    ..Default::default()
                },
                |context| {
                    let p = Palette::new(theme == egui::Theme::Dark);
                    assert_eq!(context.theme(), theme);
                    assert_eq!(context.style().visuals.window_fill, p.surface);
                    assert_eq!(context.style().visuals.text_color(), p.text);
                },
            );
        }
    }

    #[test]
    fn appearance_persists_separately_without_changing_game_settings() -> Result<()> {
        let root = tempfile::tempdir()?;
        let game = root.path().join("settings.json");
        fs::write(&game, b"existing game settings")?;
        let mut store = AppearanceStore::load(root.path())?;
        assert_eq!(store.appearance, Appearance::System);
        assert!(!root.path().join("interface.json").exists());
        store.set(Appearance::Light)?;
        assert_eq!(
            AppearanceStore::load(root.path())?.appearance,
            Appearance::Light
        );
        assert_eq!(fs::read(game)?, b"existing game settings");
        fs::write(
            root.path().join("interface.json"),
            br#"{"version":99,"appearance":"dark"}"#,
        )?;
        assert!(AppearanceStore::load(root.path()).is_err());
        Ok(())
    }
}
