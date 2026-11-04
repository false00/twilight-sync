use super::{Palette, semibold};
use crate::history;
use eframe::egui::{self, Align2, FontId, Pos2, Stroke, Vec2};
use std::collections::HashMap;

pub(super) fn fork_diagram(ui: &mut egui::Ui, base: Option<&str>) {
    let palette = Palette::of(ui);
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 104.0), egui::Sense::hover());
    let root = Pos2::new(rect.center().x, rect.top() + 29.0);
    let local = Pos2::new(rect.left() + rect.width() * 0.25, rect.top() + 70.0);
    let remote = Pos2::new(rect.left() + rect.width() * 0.75, rect.top() + 70.0);
    let painter = ui.painter();
    if let Some(hash) = base {
        painter.text(
            Pos2::new(root.x, rect.top()),
            Align2::CENTER_TOP,
            format!("Last synced {}", hash.get(..8).unwrap_or(hash)),
            FontId::proportional(12.0),
            palette.secondary,
        );
        for (position, color) in [(local, palette.accent), (remote, palette.warning)] {
            painter.add(egui::Shape::line(
                vec![root, Pos2::new(root.x, root.y + 12.0), position],
                Stroke::new(2.0_f32, color),
            ));
        }
        painter.circle_filled(root, 4.0, palette.secondary);
    } else {
        painter.text(
            Pos2::new(root.x, rect.top()),
            Align2::CENTER_TOP,
            "No verified common save",
            FontId::proportional(12.0),
            palette.secondary,
        );
    }
    for (position, label, color) in [
        (local, "This PC", palette.accent),
        (remote, "Partner", palette.warning),
    ] {
        painter.circle_filled(position, 6.0, color);
        painter.text(
            position + Vec2::new(0.0, 12.0),
            Align2::CENTER_TOP,
            label,
            semibold(13.0),
            color,
        );
    }
}

pub(super) fn history_graph(
    ui: &mut egui::Ui,
    entries: &[history::Entry],
    nodes: &HashMap<String, Pos2>,
    current: Option<&str>,
) {
    let palette = Palette::of(ui);
    for entry in entries {
        let Some(position) = nodes.get(&entry.id) else {
            continue;
        };
        for parent in [&entry.parent, &entry.merge_parent].into_iter().flatten() {
            if let Some(parent) = nodes.get(parent) {
                let color = if entry.merge_parent.is_some() {
                    palette.success
                } else if entry.reason == history::Reason::PeerBranch {
                    palette.warning
                } else {
                    palette.accent
                };
                ui.painter().add(egui::Shape::line(
                    vec![
                        *position,
                        Pos2::new(position.x, position.y + 14.0),
                        Pos2::new(parent.x, position.y + 14.0),
                        *parent,
                    ],
                    Stroke::new(1.5_f32, color),
                ));
            }
        }
    }
    for entry in entries {
        if let Some(position) = nodes.get(&entry.id) {
            let color = if current == Some(entry.hash.as_str()) {
                palette.success
            } else if entry.reason == history::Reason::PeerBranch {
                palette.warning
            } else {
                palette.accent
            };
            ui.painter().circle_filled(*position, 5.0, color);
            ui.painter()
                .circle_stroke(*position, 8.0, Stroke::new(1.0_f32, color));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::game_view;
    use super::*;
    use crate::{
        runtime::{Command, SaveConflict},
        save_data,
    };
    use anyhow::Result;

    #[test]
    fn progress_and_comparison_fit_light_dark_and_narrow_layouts() -> Result<()> {
        let backend = crate::runtime::ui_smoke_backend("ready")?;
        let mut snapshot = backend.snapshot()?;
        backend.command(crate::runtime::Command::Shutdown)?;
        let summary = save_data::inspect(&save_data::tests::populated(true)?)?;
        snapshot.local_save = Some(summary.clone());
        snapshot.peer_name = Some("Partner's handheld gaming PC".into());
        snapshot.conflict = Some(SaveConflict {
            local_hash: "a".repeat(64),
            remote_hash: "b".repeat(64),
            base_hash: Some("c".repeat(64)),
            base: Some(summary.clone()),
            remote: Some(summary),
        });
        for width in [280.0, 520.0, 720.0, 960.0] {
            for dark in [false, true] {
                let context = egui::Context::default();
                super::super::design::install(&context)?;
                context.set_visuals(if dark {
                    egui::Visuals::dark()
                } else {
                    egui::Visuals::light()
                });
                let output = context.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            Pos2::ZERO,
                            Vec2::new(width, 2000.0),
                        )),
                        ..Default::default()
                    },
                    |context| {
                        egui::CentralPanel::default().show(context, |ui| {
                            game_view::comparison(
                                ui,
                                &game_view::view(crate::protocol::GameId::Dusklight, &snapshot),
                                &mut None,
                            );
                            if let Some(summary) = &snapshot.local_save {
                                game_view::progress(ui, &game_view::dusklight_cards(summary));
                            }
                        });
                    },
                );
                let mut text_count = 0;
                for clipped in &output.shapes {
                    if let egui::Shape::Text(text) = &clipped.shape {
                        let bounds = text.galley.rect.translate(text.pos.to_vec2());
                        assert!(
                            bounds.right() <= width + 1.0,
                            "Overflow at {width}px: {}",
                            text.galley.job.text
                        );
                        assert!(
                            bounds.left() >= -1.0,
                            "Negative text position: {}",
                            text.galley.job.text
                        );
                        text_count += 1;
                    }
                }
                assert!(text_count > 20, "Progress view was blank");
            }
        }
        Ok(())
    }

    #[test]
    fn full_conflict_dialog_keeps_text_and_actions_within_short_and_high_dpi_windows() -> Result<()>
    {
        let backend = crate::runtime::ui_smoke_backend("ready")?;
        let mut snapshot = backend.snapshot()?;
        backend.command(Command::Shutdown)?;
        let summary = save_data::inspect(&save_data::tests::populated(true)?)?;
        snapshot.local_save = Some(summary.clone());
        snapshot.conflict = Some(SaveConflict {
            local_hash: "a".repeat(64),
            remote_hash: "b".repeat(64),
            base_hash: None,
            base: None,
            remote: Some(summary),
        });
        for size in [
            Vec2::new(320.0, 230.0),
            Vec2::new(426.0, 306.0),
            Vec2::new(640.0, 460.0),
            Vec2::new(1080.0, 760.0),
        ] {
            for dark in [false, true] {
                for selected in [None, Some(false), Some(true)] {
                    let context = egui::Context::default();
                    super::super::design::install(&context)?;
                    context.set_visuals(if dark {
                        egui::Visuals::dark()
                    } else {
                        egui::Visuals::light()
                    });
                    let mut choice = selected;
                    for _ in 0..2 {
                        let output = context.run(
                            egui::RawInput {
                                screen_rect: Some(egui::Rect::from_min_size(Pos2::ZERO, size)),
                                ..Default::default()
                            },
                            |context| {
                                let (_, command) = game_view::prompt(
                                    context,
                                    &game_view::view(crate::protocol::GameId::Dusklight, &snapshot),
                                    &mut choice,
                                );
                                assert!(
                                    command.is_none(),
                                    "Layout must never select or apply a branch"
                                );
                            },
                        );
                        for clipped in &output.shapes {
                            if let egui::Shape::Text(text) = &clipped.shape {
                                let bounds = text.galley.rect.translate(text.pos.to_vec2());
                                if clipped.clip_rect.intersects(bounds) {
                                    assert!(
                                        bounds.right() <= size.x + 1.0 && bounds.left() >= -1.0,
                                        "Dialog overflow at {size:?}: {}",
                                        text.galley.job.text
                                    );
                                }
                                if size.y >= 400.0 && text.galley.job.text == "Not now" {
                                    assert!(
                                        bounds.bottom() <= size.y && bounds.top() >= 0.0,
                                        "Dialog actions left viewport"
                                    );
                                }
                            }
                        }
                    }
                    assert_eq!(choice, selected, "No default winner may be chosen");
                }
            }
        }
        Ok(())
    }
}
