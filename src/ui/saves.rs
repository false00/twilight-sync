use super::{Dashboard, Palette, semibold, symbol};
use crate::{
    history,
    runtime::{Command, Snapshot},
    save_data::{self, Summary},
};
use eframe::egui::{self, Align2, Color32, FontId, Pos2, RichText, Stroke, Vec2};
use std::collections::HashMap;

pub(super) fn brief(ui: &mut egui::Ui, summary: &Summary) {
    if summary.slots.is_empty() {
        Dashboard::secondary(ui, "No occupied quest slots");
    }
    for slot in &summary.slots {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("Slot {} / {}", slot.number, slot.player)).strong());
            ui.label(symbol("\u{eb52}", 13.0).color(Palette::of(ui).error));
            ui.label(format!("{} hearts", slot.hearts));
            if let Some(seconds) = slot.play_seconds {
                ui.label(save_data::duration_label(seconds));
            }
            ui.label(format!("{} rupees", slot.rupees));
        });
    }
}

fn metric(ui: &mut egui::Ui, value: String, label: &str, color: Color32) {
    let width = 118.0_f32.min(ui.available_width());
    ui.allocate_ui_with_layout(
        Vec2::new(width, 48.0),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            ui.label(RichText::new(value).font(semibold(18.0)).color(color));
            ui.label(
                RichText::new(label)
                    .small()
                    .color(Palette::of(ui).secondary),
            );
        },
    );
}

fn inventory(ui: &mut egui::Ui, summary: &Summary) {
    let palette = Palette::of(ui);
    for slot in &summary.slots {
        if summary.slots.len() > 1 {
            ui.label(RichText::new(format!("Slot {}", slot.number)).strong());
        }
        for equipment in &slot.equipment {
            ui.horizontal_wrapped(|ui| {
                ui.label(symbol("\u{e73e}", 12.0).color(palette.success));
                ui.label(equipment);
            });
        }
        for item in &slot.items {
            ui.horizontal_wrapped(|ui| {
                ui.label(symbol("\u{e7fc}", 12.0).color(palette.accent));
                ui.add(egui::Label::new(&item.name).wrap())
                    .on_hover_text(format!("Save item ID: 0x{:02X}", item.id));
            });
        }
        Dashboard::secondary(
            ui,
            format!(
                "Arrows: {} | Bomb bags: {}, {}, {}",
                slot.arrows, slot.bombs[0], slot.bombs[1], slot.bombs[2]
            ),
        );
    }
}

pub(super) fn progress(ui: &mut egui::Ui, summary: &Summary) {
    let palette = Palette::of(ui);
    if summary.slots.is_empty() {
        Dashboard::secondary(ui, "No occupied quest slots");
    }
    for slot in &summary.slots {
        ui.push_id(slot.number, |ui| {
            ui.label(
                RichText::new(format!("Slot {} / {}", slot.number, slot.player))
                    .font(semibold(16.0)),
            );
            Dashboard::secondary(ui, format!("Horse: {}", slot.horse));
            if let Some(saved) = &slot.saved_at {
                Dashboard::secondary(ui, format!("Saved (UTC): {saved}"));
            } else {
                Dashboard::secondary(ui, "Save clock unavailable");
            }
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                for _ in 0..slot.hearts.min(20) {
                    ui.label(symbol("\u{eb52}", 16.0).color(palette.error))
                        .on_hover_text("Heart container capacity");
                }
            });
            Dashboard::secondary(
                ui,
                format!(
                    "{} containers | {}/5 heart pieces | {:.2} health",
                    slot.hearts,
                    slot.heart_pieces,
                    f32::from(slot.life_quarters) / 4.0
                ),
            );
            ui.horizontal_wrapped(|ui| {
                metric(
                    ui,
                    slot.play_seconds
                        .map(save_data::duration_label)
                        .unwrap_or_else(|| "Unavailable".into()),
                    "Played",
                    palette.accent,
                );
                metric(ui, slot.rupees.to_string(), "Rupees", palette.success);
                metric(ui, format!("{} / 60", slot.poes), "Poe souls", palette.text);
                metric(
                    ui,
                    format!("{} / 24", slot.insects),
                    "Golden insects",
                    palette.text,
                );
                metric(
                    ui,
                    format!("{} / 4", slot.fused_shadows),
                    "Fused Shadow pieces",
                    palette.text,
                );
                metric(
                    ui,
                    format!("{} / 4", slot.mirror_shards),
                    "Mirror shards",
                    palette.text,
                );
            });
            ui.add(egui::Label::new(slot.story()).wrap());
            if !slot.stage.is_empty() {
                Dashboard::secondary(
                    ui,
                    format!(
                        "Save area: {} / room {} | {}",
                        slot.stage,
                        slot.room,
                        if slot.wolf { "Wolf form" } else { "Human form" }
                    ),
                );
            }
            Dashboard::secondary(
                ui,
                format!("Deaths: {} | Clear count: {}", slot.deaths, slot.clears),
            );
            ui.add_space(6.0);
            if summary.slots.len() > 1 {
                ui.separator();
            }
        });
    }
    ui.collapsing("Equipment and inventory", |ui| inventory(ui, summary));
}

pub(super) fn changes(ui: &mut egui::Ui, changes: &[String]) {
    for change in changes.iter().take(5) {
        ui.add(
            egui::Label::new(RichText::new(format!("- {change}")).color(Palette::of(ui).secondary))
                .wrap(),
        );
    }
    if changes.len() > 5 {
        ui.collapsing(format!("{} more changes", changes.len() - 5), |ui| {
            for change in changes.iter().skip(5) {
                ui.add(egui::Label::new(format!("- {change}")).wrap());
            }
        });
    }
}

fn fork_diagram(ui: &mut egui::Ui, base: Option<&str>) {
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
                Stroke::new(2.0, color),
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

fn branch(ui: &mut egui::Ui, snapshot: &Snapshot, choice: &mut Option<bool>, remote: bool) {
    let palette = Palette::of(ui);
    let Some(conflict) = &snapshot.conflict else {
        return;
    };
    ui.push_id(
        if remote {
            "remote-branch"
        } else {
            "local-branch"
        },
        |ui| {
            let title = if remote {
                snapshot
                    .peer_name
                    .as_deref()
                    .or_else(|| {
                        snapshot
                            .settings
                            .peer
                            .as_ref()
                            .map(|peer| peer.name.as_str())
                    })
                    .unwrap_or("Partner")
            } else {
                "This PC"
            };
            if ui
                .add(egui::RadioButton::new(
                    *choice == Some(remote),
                    RichText::new(title).font(semibold(17.0)).color(if remote {
                        palette.warning
                    } else {
                        palette.accent
                    }),
                ))
                .clicked()
            {
                *choice = Some(remote);
            }
            let hash = if remote {
                &conflict.remote_hash
            } else {
                &conflict.local_hash
            };
            Dashboard::secondary(ui, format!("Save {}", hash.get(..12).unwrap_or(hash)));
            let summary = if remote {
                conflict.remote.as_ref()
            } else {
                snapshot.local_save.as_ref()
            };
            if let Some(summary) = summary {
                progress(ui, summary);
                let baseline = conflict.base.as_ref().or(if remote {
                    snapshot.local_save.as_ref()
                } else {
                    conflict.remote.as_ref()
                });
                if let Some(baseline) = baseline {
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(if conflict.base.is_some() {
                            "Since last sync"
                        } else {
                            "Compared with other branch"
                        })
                        .strong(),
                    );
                    let differences = summary.changes_from(baseline);
                    if differences.is_empty() {
                        Dashboard::secondary(
                            ui,
                            "Other save data differs; no decoded progress differences.",
                        );
                    } else {
                        changes(ui, &differences);
                    }
                }
            } else {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Waiting for verified save details...");
                });
            }
        },
    );
}

pub(super) fn comparison(ui: &mut egui::Ui, snapshot: &Snapshot, choice: &mut Option<bool>) {
    let Some(conflict) = &snapshot.conflict else {
        return;
    };
    fork_diagram(ui, conflict.base_hash.as_deref());
    if ui.available_width() >= 680.0 {
        ui.columns(2, |columns| {
            branch(&mut columns[0], snapshot, choice, false);
            branch(&mut columns[1], snapshot, choice, true);
        });
    } else {
        branch(ui, snapshot, choice, false);
        ui.add_space(12.0);
        ui.separator();
        branch(ui, snapshot, choice, true);
    }
    if conflict.remote.is_some() {
        ui.label(
            RichText::new("Both complete versions are preserved in save history.")
                .color(Palette::of(ui).success),
        );
    }
}

fn prompt_contents(
    ui: &mut egui::Ui,
    snapshot: &Snapshot,
    choice: &mut Option<bool>,
    availability: (bool, bool),
    comparison_height: Option<f32>,
) -> (bool, Option<Command>) {
    let Some(conflict) = &snapshot.conflict else {
        return (true, None);
    };
    let (can_play, can_resolve) = availability;
    ui.label(RichText::new("Choose a save branch").font(semibold(21.0)));
    ui.label("These PCs have different progress. Neither save has been replaced.");
    if let Some(height) = comparison_height {
        egui::ScrollArea::vertical()
            .id_salt("fork-comparison-scroll")
            .max_height(height)
            .auto_shrink([false, false])
            .show(ui, |ui| comparison(ui, snapshot, choice));
    } else {
        comparison(ui, snapshot, choice);
    }
    ui.separator();
    ui.label(RichText::new("The selected complete save includes all three quest slots. Originals remain in history.")
        .small().color(Palette::of(ui).secondary));
    let title = match choice {
        Some(true) => "Use partner's save on this PC",
        Some(false) => "Offer this PC's save to partner",
        None => "Select a branch above",
    };
    let mut close = false;
    let mut command = None;
    ui.horizontal_wrapped(|ui| {
        if Dashboard::primary(ui, can_resolve && choice.is_some(), title).clicked()
            && let Some(use_remote) = *choice
        {
            command = Some(Command::ResolveConflict {
                local_hash: conflict.local_hash.clone(),
                remote_hash: conflict.remote_hash.clone(),
                use_remote,
            });
            close = true;
        }
        if ui
            .add_enabled(can_play, egui::Button::new("Keep both; play this PC"))
            .clicked()
        {
            command = Some(Command::ContinueLocal {
                expected: Some(conflict.local_hash.clone()),
            });
            close = true;
        }
        if ui.button("Not now").clicked() {
            close = true;
        }
    });
    if snapshot.game_running || snapshot.remote_game {
        Dashboard::secondary(ui, "Stop running games before choosing.");
    } else if snapshot.settings.play_lock.is_some() {
        Dashboard::secondary(ui, "Recover the previous session before choosing.");
    } else if !snapshot.transfer.is_empty() {
        Dashboard::secondary(ui, "Finish the current transfer before choosing.");
    } else if let Some(remote) = &conflict.remote
        && snapshot
            .local_save
            .as_ref()
            .is_some_and(|local| local.game != remote.game || local.file_name != remote.file_name)
    {
        ui.label(
            RichText::new(
                "Different game regions or save names. These saves cannot replace each other.",
            )
            .color(Palette::of(ui).warning),
        );
    } else if *choice == Some(false) {
        Dashboard::secondary(
            ui,
            "Your partner must approve before their save is replaced.",
        );
    }
    (close, command)
}

pub(super) fn prompt(
    context: &egui::Context,
    snapshot: &Snapshot,
    choice: &mut Option<bool>,
    can_play: bool,
    can_resolve: bool,
) -> (bool, Option<Command>) {
    let height = context.content_rect().height();
    let response = egui::Modal::new(egui::Id::new("save-fork-comparison")).show(context, |ui| {
        ui.set_width((context.content_rect().width() - 64.0).clamp(260.0, 900.0));
        if height < 400.0 {
            egui::ScrollArea::vertical()
                .id_salt("short-fork-dialog")
                .max_height((height - 64.0).max(80.0))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    prompt_contents(ui, snapshot, choice, (can_play, can_resolve), None)
                })
                .inner
        } else {
            prompt_contents(
                ui,
                snapshot,
                choice,
                (can_play, can_resolve),
                Some((height - 280.0).max(40.0)),
            )
        }
    });
    (
        response.inner.0 || response.should_close(),
        response.inner.1,
    )
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
                    Stroke::new(1.5, color),
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
                .circle_stroke(*position, 8.0, Stroke::new(1.0, color));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::SaveConflict;
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
                            comparison(ui, &snapshot, &mut None);
                            if let Some(summary) = &snapshot.local_save {
                                inventory(ui, summary);
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
                                let (_, command) =
                                    prompt(context, &snapshot, &mut choice, true, true);
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
