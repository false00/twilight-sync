use super::{Dashboard, Palette, semibold, symbol};
use crate::{
    protocol::GameId,
    runtime::{Command, Snapshot},
    save_data, totk,
};
use eframe::egui::{self, RichText, Stroke, Vec2};

#[derive(Clone, Debug)]
pub(super) struct SaveCard {
    pub number: u8,
    pub title: String,
    pub kind: String,
    pub saved: Option<String>,
    pub hearts: Option<f32>,
    pub rupees: Option<u32>,
    pub played: Option<u64>,
    pub details: Vec<(String, String)>,
    pub inventory: Vec<String>,
    pub warning: Option<String>,
}

pub(super) fn dusklight_cards(summary: &save_data::Summary) -> Vec<SaveCard> {
    summary
        .slots
        .iter()
        .map(|slot| {
            let mut details = vec![
                ("Player".into(), slot.player.clone()),
                ("Horse".into(), slot.horse.clone()),
                (
                    "Location".into(),
                    format!("{} / room {}", slot.stage, slot.room),
                ),
                (
                    "Form".into(),
                    if slot.wolf { "Wolf" } else { "Human" }.into(),
                ),
                ("Heart pieces".into(), format!("{} / 5", slot.heart_pieces)),
                ("Poe souls".into(), format!("{} / 60", slot.poes)),
                ("Golden insects".into(), format!("{} / 24", slot.insects)),
                (
                    "Fused Shadows".into(),
                    format!("{} / 4", slot.fused_shadows),
                ),
                (
                    "Mirror shards".into(),
                    format!("{} / 4", slot.mirror_shards),
                ),
                ("Deaths".into(), slot.deaths.to_string()),
                ("Clears".into(), slot.clears.to_string()),
                ("Story".into(), slot.story()),
                (
                    "Ammunition".into(),
                    format!(
                        "{} arrows / bombs {}, {}, {}",
                        slot.arrows, slot.bombs[0], slot.bombs[1], slot.bombs[2]
                    ),
                ),
            ];
            details.retain(|(_, value)| !value.is_empty());
            let mut inventory = slot.equipment.clone();
            inventory.extend(slot.items.iter().map(|item| item.name.clone()));
            SaveCard {
                number: slot.number,
                title: format!("Slot {}", slot.number),
                kind: slot.player.clone(),
                saved: slot.saved_at.as_ref().map(|saved| format!("{saved} UTC")),
                hearts: Some(f32::from(slot.hearts)),
                rupees: Some(u32::from(slot.rupees)),
                played: slot.play_seconds,
                details,
                inventory,
                warning: None,
            }
        })
        .collect()
}

pub(super) fn totk_cards(summary: &totk::Summary) -> Vec<SaveCard> {
    let mut cards: Vec<_> = summary
        .slots
        .iter()
        .map(|slot| {
            let mut card = SaveCard {
                number: slot.number + 1,
                title: format!("Slot {}", slot.number + 1),
                kind: "Save slot".into(),
                saved: None,
                hearts: None,
                rupees: None,
                played: None,
                details: vec![
                    ("Files".into(), slot.files.to_string()),
                    (
                        "Snapshot size".into(),
                        format!("{:.1} MiB", slot.bytes as f64 / 1_048_576.0),
                    ),
                ],
                inventory: Vec::new(),
                warning: slot.details_error.clone(),
            };
            if let Some(details) = &slot.details {
                card.kind = match details.autosave {
                    Some(true) => "Autosave",
                    Some(false) => "Manual save",
                    None => "Save slot",
                }
                .into();
                card.saved = details
                    .saved_at
                    .as_ref()
                    .map(|saved| format!("{saved} (game clock)"));
                card.hearts = details.max_hearts;
                card.rupees = details.rupees;
                card.played = details.play_seconds.map(u64::from);
                if let Some(stamina) = details.stamina_wheels {
                    card.details
                        .push(("Stamina".into(), format!("{stamina:.1} wheels")));
                }
                if let Some(map) = &details.map {
                    card.details.push(("Location".into(), map.clone()));
                }
                if let Some([east, altitude, north]) = details.position {
                    card.details.push((
                        "World position".into(),
                        format!("{east:.0}, {altitude:.0}, {north:.0}"),
                    ));
                }
                card.details
                    .push(("Save format".into(), details.format.clone()));
            }
            card
        })
        .collect();
    cards.sort_by(|left, right| {
        right
            .saved
            .cmp(&left.saved)
            .then(left.number.cmp(&right.number))
    });
    cards
}

pub(super) fn title(game: GameId) -> &'static str {
    match game {
        GameId::Dusklight => "Dusklight",
        GameId::Totk => "Tears of the Kingdom",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    Play,
    Sync,
    Compare,
    History,
    Refresh,
    Setup,
    Recover,
    Locate,
    Home,
    Devices,
    Incoming(bool),
    Recovery(bool),
}

pub(super) fn incoming_command(game: GameId, approved: bool) -> Command {
    match game {
        GameId::Dusklight => Command::AcceptSave(approved),
        GameId::Totk => Command::AcceptTotk(approved),
    }
}

pub(super) struct GameView {
    pub game: GameId,
    pub title: &'static str,
    pub partner: String,
    pub installed: bool,
    pub executable: Option<std::path::PathBuf>,
    pub can_locate: bool,
    pub matching: bool,
    pub conflict: bool,
    pub running: bool,
    pub remote_running: bool,
    pub reserved: bool,
    pub pending: bool,
    pub local_hash: Option<String>,
    pub remote_hash: Option<String>,
    pub local: Vec<SaveCard>,
    pub remote: Vec<SaveCard>,
    pub headline: String,
    pub message: String,
    pub play_reason: Option<String>,
    pub sync_reason: Option<String>,
    pub verified: Option<String>,
    pub base: Option<String>,
    pub can_resolve: bool,
    pub can_continue: bool,
    pub can_accept: bool,
    pub recovery_pending: bool,
    pub connection_setup: Option<&'static str>,
}

fn dusklight_reason(snapshot: &Snapshot) -> Option<String> {
    let reason = if snapshot.game_running {
        Some("The game is running on this PC.")
    } else if snapshot.remote_game {
        Some("The paired PC is playing.")
    } else if snapshot.pending_save {
        Some("An incoming save is waiting for your decision.")
    } else if snapshot.pending_recovery {
        Some("A session recovery is waiting for your decision.")
    } else if snapshot.settings.play_lock.is_some() {
        Some("A stored play reservation needs completed-session recovery.")
    } else if snapshot.install_busy || snapshot.texture_busy || snapshot.self_updates.preparing {
        Some("Finish the current installation or update.")
    } else if snapshot.pending_iso || !snapshot.transfer.is_empty() {
        Some("Finish or decline the pending transfer.")
    } else {
        None
    };
    reason.map(str::to_owned)
}

pub(super) fn view(game: GameId, snapshot: &Snapshot) -> GameView {
    let partner = snapshot
        .peer_name
        .as_deref()
        .or_else(|| {
            snapshot
                .settings
                .peer
                .as_ref()
                .map(|peer| peer.name.as_str())
        })
        .unwrap_or("Paired PC")
        .to_owned();
    let trusted = snapshot.network.as_ref().is_some_and(|network| {
        snapshot.settings.network.as_deref() == Some(network.fingerprint.as_str())
    });
    let connection_setup = if snapshot.ready {
        None
    } else if !trusted {
        Some("Network approval needed.")
    } else if snapshot.settings.peer.is_none() {
        Some("No trusted partner is paired.")
    } else {
        None
    };
    let mut model = match game {
        GameId::Dusklight => {
            let installed = snapshot.settings.game_exe.is_some();
            let conflict = snapshot.conflict.is_some();
            let matching = snapshot.ready
                && snapshot.save_ready
                && !snapshot.game_running
                && !snapshot.remote_game
                && snapshot.local_hash.is_some()
                && snapshot.local_hash == snapshot.remote_hash;
            let mut play_reason = dusklight_reason(snapshot);
            if play_reason.is_none() && !installed {
                play_reason = Some("Locate or install the game on the Games page.".into());
            }
            if play_reason.is_none() && (!snapshot.save_ready || snapshot.save_profile.is_none()) {
                play_reason = Some("The active save profile is not verified. Check the current action-needed message.".into());
            }
            if play_reason.is_none() && snapshot.settings.save_file.is_none() {
                play_reason = Some("The active save location is not verified and ready.".into());
            }
            if play_reason.is_none() && conflict {
                play_reason = Some("Compare both branches or keep both before playing.".into());
            }
            if play_reason.is_none() && !Dashboard::can_launch(snapshot) {
                play_reason = Some(
                    "The active save profile or texture state needs recovery before playing."
                        .into(),
                );
            }
            let sync_reason = dusklight_reason(snapshot).or_else(|| {
                if !snapshot.ready {
                    Some("The paired PC is offline. Local progress is retained.".into())
                } else if !snapshot.save_ready {
                    Some("The local save is not verified and ready.".into())
                } else if snapshot.local_hash.is_none() {
                    Some("There is no committed save on this PC to send.".into())
                } else if matching {
                    Some("Both save snapshots already match. There is nothing to send.".into())
                } else {
                    None
                }
            });
            GameView {
                game,
                title: title(game),
                partner,
                installed,
                matching,
                conflict,
                executable: snapshot.settings.game_exe.clone(),
                can_locate: Dashboard::management_available(snapshot),
                running: snapshot.game_running,
                remote_running: snapshot.remote_game,
                reserved: snapshot.settings.play_lock.is_some(),
                pending: snapshot.pending_save,
                local_hash: snapshot.local_hash.clone(),
                remote_hash: snapshot.remote_hash.clone(),
                local: snapshot
                    .local_save
                    .as_ref()
                    .map(dusklight_cards)
                    .unwrap_or_default(),
                remote: snapshot
                    .conflict
                    .as_ref()
                    .and_then(|conflict| conflict.remote.as_ref())
                    .map(dusklight_cards)
                    .unwrap_or_default(),
                headline: String::new(),
                message: snapshot.message.clone(),
                play_reason,
                sync_reason,
                verified: None,
                base: snapshot
                    .conflict
                    .as_ref()
                    .and_then(|conflict| conflict.base_hash.clone()),
                can_resolve: Dashboard::can_reconcile(snapshot),
                can_continue: Dashboard::local_play_available(snapshot, true),
                can_accept: Dashboard::management_available(snapshot)
                    && snapshot.save_ready
                    && !snapshot.pending_iso,
                recovery_pending: snapshot.pending_recovery,
                connection_setup,
            }
        }
        GameId::Totk => {
            let state = &snapshot.totk;
            GameView {
                game,
                title: title(game),
                partner,
                installed: state.installed,
                executable: state
                    .install_path
                    .as_ref()
                    .map(|path| path.join("totk.exe")),
                can_locate: !state.running
                    && !state.remote_running
                    && !state.busy
                    && state.lock.is_none()
                    && state.remote_lock.is_none()
                    && !snapshot.self_updates.preparing,
                matching: snapshot.ready
                    && state.remote_save_ready
                    && state.verification_error.is_none()
                    && !state.running
                    && !state.remote_running
                    && state.local_hash.is_some()
                    && state.local_hash == state.remote_hash,
                conflict: state.conflict,
                running: state.running,
                remote_running: state.remote_running,
                reserved: state.lock.is_some() || state.remote_lock.is_some(),
                pending: state.pending,
                local_hash: state.local_hash.clone(),
                remote_hash: state.remote_hash.clone(),
                local: state
                    .local_summary
                    .as_ref()
                    .map(totk_cards)
                    .unwrap_or_default(),
                remote: state
                    .remote_summary
                    .as_ref()
                    .filter(|_| state.comparison_verified)
                    .map(totk_cards)
                    .unwrap_or_default(),
                headline: String::new(),
                message: state.message.clone(),
                play_reason: Dashboard::totk_play_blocker(snapshot).map(str::to_owned),
                sync_reason: Dashboard::totk_sync_blocker(snapshot).map(str::to_owned),
                verified: state.verified_at.clone(),
                base: None,
                can_resolve: snapshot.ready
                    && state.comparison_verified
                    && !state.busy
                    && !state.running
                    && !state.remote_running
                    && state.lock.is_none()
                    && state.remote_lock.is_none()
                    && !snapshot.self_updates.preparing,
                can_continue: state.installed
                    && !state.running
                    && !state.remote_running
                    && (!state.busy || state.pending)
                    && !state.launch_pending
                    && state.lock.is_none()
                    && state.remote_lock.is_none()
                    && state.verification_error.is_none()
                    && !snapshot.self_updates.preparing,
                can_accept: !state.running
                    && !state.remote_running
                    && !snapshot.self_updates.preparing
                    && state.verification_error.is_none()
                    && state.lock.is_none()
                    && state.remote_lock.is_none(),
                recovery_pending: state.recovery_pending,
                connection_setup,
            }
        }
    };
    model.headline = if model.running {
        "Playing on this PC"
    } else if model.remote_running {
        "Partner is playing"
    } else if model.recovery_pending {
        "Session recovery needs approval"
    } else if model.pending {
        "Incoming save needs approval"
    } else if model.conflict {
        "Choose a save branch"
    } else if model.reserved {
        "Previous session needs attention"
    } else if model.matching {
        "Saves synchronized"
    } else if !model.installed {
        "Game setup needed"
    } else if !snapshot.ready {
        "Offline play"
    } else if model.local_hash.is_none() {
        "No committed save yet"
    } else {
        "Save sync status"
    }
    .into();
    model.local.sort_by(|left, right| {
        right
            .saved
            .cmp(&left.saved)
            .then(left.number.cmp(&right.number))
    });
    model
}

fn command_button(
    ui: &mut egui::Ui,
    width: f32,
    enabled: bool,
    primary: bool,
    label: &str,
    reason: Option<&str>,
) -> bool {
    let palette = Palette::of(ui);
    let mut button = egui::Button::new(RichText::new(label).color(if primary && enabled {
        palette.on_accent
    } else if enabled {
        palette.text
    } else {
        palette.secondary
    }))
    .min_size(Vec2::new(width.min(ui.available_width()), 34.0));
    if primary {
        button = button.fill(if enabled {
            palette.accent
        } else {
            palette.disabled
        });
    }
    ui.add_enabled(enabled, button)
        .on_disabled_hover_text(reason.unwrap_or_default())
        .clicked()
}

pub(super) fn toolbar(ui: &mut egui::Ui, model: &GameView) -> Option<Action> {
    let mut action = None;
    let width = ui.available_width();
    ui.horizontal_wrapped(|ui| {
        if command_button(
            ui,
            238.0_f32.min(width),
            model.play_reason.is_none(),
            true,
            &format!("Play {}", model.title),
            model.play_reason.as_deref(),
        ) {
            action = Some(Action::Play);
        }
        if command_button(
            ui,
            108.0,
            model.sync_reason.is_none(),
            false,
            "Sync now",
            model.sync_reason.as_deref(),
        ) {
            action = Some(Action::Sync);
        }
        if command_button(
            ui,
            108.0,
            model.conflict && !model.running && !model.remote_running,
            false,
            "Compare saves",
            Some("There are no different save branches to compare, or a game is still running."),
        ) {
            action = Some(Action::Compare);
        }
        if command_button(ui, 92.0, true, false, "History", None) {
            action = Some(Action::History);
        }
        if ui
            .add_sized([34.0, 34.0], egui::Button::new(symbol("\u{e72c}", 17.0)))
            .on_hover_text("Refresh this game's save status")
            .clicked()
        {
            action = Some(Action::Refresh);
        }
    });
    for (label, reason) in [("Play", &model.play_reason), ("Sync", &model.sync_reason)] {
        if let Some(reason) = reason {
            Dashboard::secondary(ui, format!("{label}: {reason}"));
        }
    }
    action
}

pub(super) fn home(ui: &mut egui::Ui, model: &GameView) -> Option<Action> {
    let palette = Palette::of(ui);
    ui.horizontal_wrapped(|ui| {
        ui.label(symbol("\u{e7fc}", 21.0).color(palette.accent));
        ui.label(RichText::new(model.title).font(semibold(21.0)));
    });
    Dashboard::secondary(ui, format!("Partner: {}", model.partner));
    ui.add_space(12.0);
    let mut action = toolbar(ui, model);
    if let Some(message) = model.connection_setup {
        ui.add_space(12.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(message).color(palette.warning));
            if ui.button("Open Devices").clicked() {
                action = Some(Action::Devices);
            }
        });
    }
    ui.add_space(14.0);
    ui.separator();
    ui.add_space(10.0);
    let color = if model.matching {
        palette.success
    } else if model.conflict || model.reserved || model.pending {
        palette.warning
    } else {
        palette.text
    };
    ui.label(
        RichText::new(&model.headline)
            .font(semibold(17.0))
            .color(color),
    );
    if !model.message.is_empty() {
        ui.add(egui::Label::new(&model.message).wrap());
    }
    if let Some(verified) = &model.verified {
        Dashboard::secondary(ui, format!("Last verified: {verified}"));
    }
    if !model.installed && ui.button("Open game setup").clicked() {
        action = Some(Action::Setup);
    }
    if model.reserved
        && !model.running
        && !model.remote_running
        && ui.button("Recover completed session...").clicked()
    {
        action = Some(Action::Recover);
    }
    ui.add_space(18.0);
    Dashboard::section_title(
        ui,
        if model.matching {
            "Progress on both PCs"
        } else {
            "This PC's progress"
        },
    );
    if let Some(card) = model.local.first() {
        ui.push_id((model.title, "current-save"), |ui| {
            save_card(ui, card, model.matching.then_some(true))
        });
        if model.local.len() > 1 {
            ui.add_space(8.0);
            ui.collapsing(
                format!("Other save slots ({})", model.local.len() - 1),
                |ui| progress(ui, &model.local[1..]),
            );
        }
    } else {
        Dashboard::secondary(ui, "No verified occupied save slots yet.");
    }
    if !model.matching && !model.remote.is_empty() {
        ui.add_space(18.0);
        Dashboard::section_title(ui, &format!("{}'s progress", model.partner));
        progress(ui, &model.remote);
    }
    ui.add_space(12.0);
    ui.collapsing("Snapshot verification", |ui| {
        for (label, hash) in [
            ("This PC", &model.local_hash),
            ("Partner", &model.remote_hash),
        ] {
            if let Some(hash) = hash {
                ui.horizontal_wrapped(|ui| {
                    ui.label(label);
                    ui.label(RichText::new(hash.get(..12).unwrap_or(hash)).monospace())
                        .on_hover_text(hash);
                });
            } else {
                Dashboard::secondary(ui, format!("{label}: no verified snapshot"));
            }
        }
    });
    action
}

pub(super) fn installation(ui: &mut egui::Ui, model: &GameView) -> Option<Action> {
    let palette = Palette::of(ui);
    ui.horizontal_wrapped(|ui| {
        ui.label(symbol("\u{e7fc}", 21.0).color(palette.accent));
        ui.label(RichText::new(model.title).font(semibold(21.0)));
    });
    ui.add_space(12.0);
    Dashboard::section_title(ui, "Game installation");
    ui.label(
        RichText::new(if model.installed {
            "Installed"
        } else {
            "Not configured"
        })
        .color(if model.installed {
            palette.success
        } else {
            palette.warning
        }),
    );
    if let Some(executable) = &model.executable {
        Dashboard::path(ui, executable);
    } else {
        Dashboard::secondary(ui, "No verified executable is selected.");
    }
    ui.add_space(12.0);
    let mut action = None;
    ui.horizontal_wrapped(|ui| {
        if command_button(
            ui,
            238.0,
            model.play_reason.is_none(),
            true,
            &format!("Play {}", model.title),
            model.play_reason.as_deref(),
        ) {
            action = Some(Action::Play);
        }
        if command_button(
            ui,
            180.0,
            model.can_locate,
            false,
            "Locate executable...",
            Some(
                "Finish the active session, update or transfer before changing the game location.",
            ),
        ) {
            action = Some(Action::Locate);
        }
        if ui.button("Save controls").clicked() {
            action = Some(Action::Home);
        }
    });
    if let Some(reason) = &model.play_reason {
        Dashboard::secondary(ui, format!("Play: {reason}"));
    }
    ui.add_space(16.0);
    ui.separator();
    action
}

pub(super) fn approval(ui: &mut egui::Ui, model: &GameView) -> Option<Action> {
    let mut action = None;
    let palette = Palette::of(ui);
    if model.pending {
        egui::Frame::new().fill(palette.alternate).stroke(Stroke::new(1.0_f32, palette.border))
            .corner_radius(4).inner_margin(12.0).show(ui, |ui| {
                ui.label(RichText::new(format!("{}: incoming save", model.title)).font(semibold(16.0)).color(palette.warning));
                ui.add(egui::Label::new("A complete save is awaiting your decision. Existing progress is retained before an approved replacement.").wrap());
                ui.horizontal_wrapped(|ui| {
                    if command_button(ui, 186.0, model.can_accept, true, "Accept incoming save",
                        Some("Close this game's active sessions and finish conflicting operations before replacing progress.")) { action = Some(Action::Incoming(true)); }
                    if ui.button("Keep current save").clicked() { action = Some(Action::Incoming(false)); }
                    if model.conflict && ui.button("Compare saves").clicked() { action = Some(Action::Compare); }
                });
                if !model.can_accept { Dashboard::secondary(ui, "Save replacement is paused until this game's sessions and safety checks are ready."); }
            });
        ui.add_space(12.0);
    }
    if model.recovery_pending {
        egui::Frame::new().fill(palette.alternate).stroke(Stroke::new(1.0_f32, palette.border))
            .corner_radius(4).inner_margin(12.0).show(ui, |ui| {
                ui.label(RichText::new(format!("{}: session recovery", model.title)).font(semibold(16.0)).color(palette.warning));
                ui.add(egui::Label::new("Confirm this game is closed on both PCs. A disconnected device alone is not proof its session has ended.").wrap());
                ui.horizontal_wrapped(|ui| {
                    if command_button(ui, 186.0, !model.running && !model.remote_running, true, "Approve recovery",
                        Some("Close this game on both PCs before approving recovery.")) { action = Some(Action::Recovery(true)); }
                    if ui.button("Keep session locked").clicked() { action = Some(Action::Recovery(false)); }
                });
            });
        ui.add_space(12.0);
    }
    action
}

#[derive(Clone, Copy)]
pub(super) enum CompareAction {
    Use(bool),
    Continue,
}

fn branch(ui: &mut egui::Ui, model: &GameView, choice: &mut Option<bool>, remote: bool) {
    ui.push_id(
        if remote {
            "partner-branch"
        } else {
            "local-branch"
        },
        |ui| {
            let title = if remote {
                model.partner.as_str()
            } else {
                "This PC"
            };
            if ui
                .add(egui::RadioButton::new(
                    *choice == Some(remote),
                    RichText::new(title).font(semibold(17.0)),
                ))
                .clicked()
            {
                *choice = Some(remote);
            }
            let hash = if remote {
                &model.remote_hash
            } else {
                &model.local_hash
            };
            if let Some(hash) = hash {
                Dashboard::secondary(ui, format!("Revision {}", hash.get(..12).unwrap_or(hash)));
            }
            let cards = if remote { &model.remote } else { &model.local };
            if cards.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.spinner();
                    ui.label("Waiting for verified snapshot details...");
                });
            } else {
                progress(ui, cards);
            }
        },
    );
}

pub(super) fn comparison(ui: &mut egui::Ui, model: &GameView, choice: &mut Option<bool>) {
    super::saves::fork_diagram(ui, model.base.as_deref());
    if ui.available_width() >= 680.0 {
        ui.columns(2, |columns| {
            branch(&mut columns[0], model, choice, false);
            branch(&mut columns[1], model, choice, true);
        });
    } else {
        branch(ui, model, choice, false);
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(12.0);
        branch(ui, model, choice, true);
    }
}

fn comparison_contents(
    ui: &mut egui::Ui,
    model: &GameView,
    choice: &mut Option<bool>,
    height: Option<f32>,
) -> (bool, Option<CompareAction>) {
    ui.label(RichText::new(format!("Compare {} saves", model.title)).font(semibold(21.0)));
    Dashboard::secondary(
        ui,
        "Neither live save is replaced until you confirm a complete branch.",
    );
    if let Some(height) = height {
        egui::ScrollArea::vertical()
            .id_salt("shared-branch-scroll")
            .max_height(height)
            .auto_shrink([false, false])
            .show(ui, |ui| comparison(ui, model, choice));
    } else {
        comparison(ui, model, choice);
    }
    ui.separator();
    Dashboard::secondary(
        ui,
        "The complete save includes every slot. Original branches remain in history.",
    );
    let label = match choice {
        Some(true) => "Use partner's save",
        Some(false) => "Offer this PC's save",
        None => "Choose a branch above",
    };
    let mut close = false;
    let mut action = None;
    ui.horizontal_wrapped(|ui| {
        if command_button(
            ui,
            198.0,
            model.can_resolve && choice.is_some(),
            true,
            label,
            Some("Both verified branches must be available and the game/session must be idle."),
        ) && let Some(remote) = *choice
        {
            action = Some(CompareAction::Use(remote));
            close = true;
        }
        if command_button(
            ui,
            198.0,
            model.can_continue,
            false,
            "Keep both; play this PC",
            Some("Finish the active session, transfer, or recovery before playing."),
        ) {
            action = Some(CompareAction::Continue);
            close = true;
        }
        if ui.button("Not now").clicked() {
            close = true;
        }
    });
    if *choice == Some(false) {
        Dashboard::secondary(
            ui,
            "Your partner must approve replacement of different progress.",
        );
    }
    if !model.can_resolve {
        Dashboard::secondary(
            ui,
            model
                .play_reason
                .as_deref()
                .unwrap_or("Waiting for both verified compatible snapshots and an idle session."),
        );
    }
    (close, action)
}

pub(super) fn prompt(
    context: &egui::Context,
    model: &GameView,
    choice: &mut Option<bool>,
) -> (bool, Option<CompareAction>) {
    let height = context.content_rect().height();
    let response =
        egui::Modal::new(egui::Id::new("shared-game-save-comparison")).show(context, |ui| {
            ui.set_width((context.content_rect().width() - 64.0).clamp(260.0, 900.0));
            if height < 400.0 {
                egui::ScrollArea::vertical()
                    .id_salt("shared-short-comparison")
                    .max_height((height - 64.0).max(80.0))
                    .auto_shrink([false, false])
                    .show(ui, |ui| comparison_contents(ui, model, choice, None))
                    .inner
            } else {
                comparison_contents(ui, model, choice, Some((height - 270.0).max(40.0)))
            }
        });
    (
        response.inner.0 || response.should_close(),
        response.inner.1,
    )
}

fn metric(ui: &mut egui::Ui, width: f32, value: &str, label: &str, color: egui::Color32) {
    ui.allocate_ui_with_layout(
        Vec2::new(width, 52.0),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            ui.add(egui::Label::new(RichText::new(value).font(semibold(18.0)).color(color)).wrap());
            Dashboard::secondary(ui, label);
        },
    );
}

pub(super) fn save_card(ui: &mut egui::Ui, card: &SaveCard, matching: Option<bool>) {
    save_card_style(ui, card, matching, true)
}

fn save_card_style(ui: &mut egui::Ui, card: &SaveCard, matching: Option<bool>, framed: bool) {
    let palette = Palette::of(ui);
    let frame = if framed {
        egui::Frame::new()
            .fill(palette.surface)
            .stroke(Stroke::new(1.0_f32, palette.border))
            .corner_radius(6)
            .inner_margin(12.0)
    } else {
        egui::Frame::NONE.inner_margin(0.0)
    };
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(&card.title).font(semibold(16.0)));
            Dashboard::secondary(ui, &card.kind);
            if let Some(matching) = matching {
                ui.label(
                    RichText::new(if matching {
                        "Matching revision"
                    } else {
                        "Different revision"
                    })
                    .small()
                    .color(if matching {
                        palette.success
                    } else {
                        palette.warning
                    }),
                );
            }
        });
        Dashboard::secondary(ui, card.saved.as_deref().unwrap_or("Save time unavailable"));
        ui.add_space(8.0);
        if let Some(hearts) = card.hearts {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for _ in 0..(hearts.floor() as usize).min(40) {
                    ui.label(symbol("\u{eb52}", 14.0).color(palette.error));
                }
            });
        }
        let width = ui.available_width();
        let columns = if width >= 420.0 {
            3
        } else if width >= 280.0 {
            2
        } else {
            1
        };
        let metric_width = ((width - ui.spacing().item_spacing.x * (columns - 1) as f32)
            / columns as f32)
            .max(64.0);
        let hearts = card
            .hearts
            .map(|hearts| {
                if hearts.fract() == 0.0 {
                    format!("{hearts:.0}")
                } else {
                    format!("{hearts:.2}")
                }
            })
            .unwrap_or_else(|| "Unavailable".into());
        let rupees = card
            .rupees
            .map(|rupees| rupees.to_string())
            .unwrap_or_else(|| "Unavailable".into());
        let played = card
            .played
            .map(save_data::duration_label)
            .unwrap_or_else(|| "Unavailable".into());
        egui::Grid::new(("shared-save-metrics", card.number))
            .num_columns(columns)
            .spacing([ui.spacing().item_spacing.x, 6.0])
            .show(ui, |ui| {
                for (index, (value, label, color)) in [
                    (hearts.as_str(), "Hearts", palette.error),
                    (rupees.as_str(), "Rupees", palette.success),
                    (played.as_str(), "Played", palette.accent),
                ]
                .into_iter()
                .enumerate()
                {
                    metric(ui, metric_width, value, label, color);
                    if (index + 1) % columns == 0 {
                        ui.end_row();
                    }
                }
            });
        if let Some(warning) = &card.warning {
            Dashboard::secondary(ui, warning);
        }
        ui.add_space(4.0);
        ui.collapsing("Save details", |ui| {
            for (label, value) in &card.details {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(format!("{label}:")).strong());
                    ui.add(egui::Label::new(value).wrap());
                });
            }
            if !card.inventory.is_empty() {
                ui.add_space(8.0);
                Dashboard::section_title(ui, "Equipment and inventory");
                for item in &card.inventory {
                    ui.add(egui::Label::new(item).wrap());
                }
            }
        });
    });
}

pub(super) fn progress(ui: &mut egui::Ui, cards: &[SaveCard]) {
    if cards.is_empty() {
        Dashboard::secondary(ui, "No occupied save slots");
    }
    for card in cards {
        ui.push_id(card.number, |ui| save_card(ui, card, None));
        ui.add_space(8.0);
    }
}

pub(super) fn history_preview(ui: &mut egui::Ui, preview: &crate::runtime::HistoryPreview) {
    let cards = match &preview.data {
        crate::runtime::HistoryPreviewData::Dusklight(summary) => dusklight_cards(summary),
        crate::runtime::HistoryPreviewData::Totk(summary) => totk_cards(summary),
    };
    for card in &cards {
        ui.push_id(card.number, |ui| save_card_style(ui, card, None, false));
        ui.add_space(8.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    fn paired_snapshot() -> anyhow::Result<Snapshot> {
        let backend = crate::runtime::ui_smoke_backend("ready")?;
        let mut snapshot = backend.snapshot()?;
        backend.command(crate::runtime::Command::Shutdown)?;
        snapshot.local_save = Some(save_data::inspect(&save_data::tests::populated(true)?)?);
        snapshot.totk.checked = true;
        snapshot.totk.installed = true;
        snapshot.totk.peer_checked = true;
        snapshot.totk.remote_installed = true;
        snapshot.totk.remote_save_ready = true;
        snapshot.totk.local_hash = Some("a".repeat(64));
        snapshot.totk.remote_hash = Some("a".repeat(64));
        snapshot.totk.local_summary = Some(totk::Summary {
            files: 4,
            bytes: 2_945_388,
            album_files: 0,
            slots: vec![totk::SlotSummary {
                number: 0,
                files: 4,
                bytes: 2_945_388,
                hash: "a".repeat(64),
                details: Some(crate::totk_save::Details {
                    format: "1.4".into(),
                    max_hearts: Some(20.0),
                    rupees: Some(999_999),
                    play_seconds: Some(1_234_567),
                    autosave: Some(false),
                    saved_at: Some("2026-11-04 14:01".into()),
                    ..Default::default()
                }),
                details_error: None,
            }],
        });
        Ok(snapshot)
    }

    #[test]
    fn both_history_previews_use_the_common_responsive_save_details() -> anyhow::Result<()> {
        let snapshot = paired_snapshot()?;
        for game in [GameId::Dusklight, GameId::Totk] {
            let data = match game {
                GameId::Dusklight => crate::runtime::HistoryPreviewData::Dusklight(
                    snapshot
                        .local_save
                        .clone()
                        .context("Synthetic Dusklight save")?,
                ),
                GameId::Totk => crate::runtime::HistoryPreviewData::Totk(
                    snapshot
                        .totk
                        .local_summary
                        .clone()
                        .context("Synthetic TotK save")?,
                ),
            };
            let preview = crate::runtime::HistoryPreview {
                game,
                id: "synthetic-preview".into(),
                data,
            };
            for width in [260.0, 480.0, 840.0] {
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
                                egui::Pos2::ZERO,
                                Vec2::new(width, 1800.0),
                            )),
                            ..Default::default()
                        },
                        |context| {
                            egui::CentralPanel::default()
                                .show(context, |ui| history_preview(ui, &preview));
                        },
                    );
                    let labels: Vec<_> = output
                        .shapes
                        .iter()
                        .filter_map(|clipped| {
                            if let egui::Shape::Text(text) = &clipped.shape {
                                let bounds = text.galley.rect.translate(text.pos.to_vec2());
                                assert!(
                                    bounds.left() >= -1.0 && bounds.right() <= width + 1.0,
                                    "{game:?}: history preview overflow: {}",
                                    text.galley.job.text
                                );
                                Some(text.galley.job.text.as_str())
                            } else {
                                None
                            }
                        })
                        .collect();
                    for expected in ["Hearts", "Rupees", "Played", "Save details"] {
                        assert!(
                            labels.contains(&expected),
                            "{game:?}: history preview missing shared detail {expected}"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn both_games_prioritize_safety_and_explain_the_same_session_states() -> anyhow::Result<()> {
        for game in [GameId::Dusklight, GameId::Totk] {
            let mut snapshot = paired_snapshot()?;
            assert_eq!(view(game, &snapshot).headline, "Saves synchronized");
            let reservation = crate::config::PlayLock {
                owner: snapshot.public_key.clone(),
                token: "r".repeat(64),
            };
            snapshot.settings.play_lock = Some(reservation.clone());
            snapshot.totk.lock = Some(reservation);
            let model = view(game, &snapshot);
            assert_eq!(model.headline, "Previous session needs attention");
            assert!(model.play_reason.is_some() && model.sync_reason.is_some());
            snapshot.pending_recovery = true;
            snapshot.totk.recovery_pending = true;
            assert_eq!(
                view(game, &snapshot).headline,
                "Session recovery needs approval"
            );
            snapshot.game_running = true;
            snapshot.totk.running = true;
            let model = view(game, &snapshot);
            assert_eq!(model.headline, "Playing on this PC");
            assert!(!model.can_accept && !model.can_resolve);
            assert!(model.play_reason.is_some() && model.sync_reason.is_some());
            snapshot.game_running = false;
            snapshot.totk.running = false;
            snapshot.remote_game = true;
            snapshot.totk.remote_running = true;
            assert_eq!(view(game, &snapshot).headline, "Partner is playing");
            snapshot = paired_snapshot()?;
            snapshot.pending_save = true;
            snapshot.totk.pending = true;
            assert_eq!(
                view(game, &snapshot).headline,
                "Incoming save needs approval"
            );
            snapshot = paired_snapshot()?;
            snapshot.ready = false;
            snapshot.settings.network = None;
            let model = view(game, &snapshot);
            assert_eq!(model.headline, "Offline play");
            assert!(model.sync_reason.is_some() && model.connection_setup.is_some());
            snapshot.settings.game_exe = None;
            snapshot.totk.installed = false;
            let model = view(game, &snapshot);
            assert_eq!(model.headline, "Game setup needed");
            assert!(model.play_reason.is_some());
        }
        Ok(())
    }

    #[test]
    fn keeping_current_progress_is_never_translated_into_replacement_consent() {
        assert!(matches!(
            incoming_command(GameId::Dusklight, false),
            Command::AcceptSave(false)
        ));
        assert!(matches!(
            incoming_command(GameId::Totk, false),
            Command::AcceptTotk(false)
        ));
        assert!(matches!(
            incoming_command(GameId::Dusklight, true),
            Command::AcceptSave(true)
        ));
        assert!(matches!(
            incoming_command(GameId::Totk, true),
            Command::AcceptTotk(true)
        ));
    }

    #[test]
    fn installation_and_approval_actions_have_the_same_responsive_contract() -> anyhow::Result<()> {
        let snapshot = paired_snapshot()?;
        for game in [GameId::Dusklight, GameId::Totk] {
            let mut model = view(game, &snapshot);
            model.pending = true;
            model.recovery_pending = true;
            for width in [320.0, 640.0, 960.0] {
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
                                egui::Pos2::ZERO,
                                Vec2::new(width, 1800.0),
                            )),
                            ..Default::default()
                        },
                        |context| {
                            egui::CentralPanel::default().show(context, |ui| {
                                assert!(installation(ui, &model).is_none());
                                assert!(approval(ui, &model).is_none());
                            });
                        },
                    );
                    let labels: Vec<_> = output
                        .shapes
                        .iter()
                        .filter_map(|clipped| {
                            if let egui::Shape::Text(text) = &clipped.shape {
                                let bounds = text.galley.rect.translate(text.pos.to_vec2());
                                assert!(
                                    bounds.left() >= -1.0 && bounds.right() <= width + 1.0,
                                    "{game:?}: installation/approval overflow: {}",
                                    text.galley.job.text
                                );
                                Some(text.galley.job.text.as_str())
                            } else {
                                None
                            }
                        })
                        .collect();
                    for expected in [
                        "Game installation",
                        "Locate executable...",
                        "Save controls",
                        "Accept incoming save",
                        "Keep current save",
                        "Approve recovery",
                        "Keep session locked",
                    ] {
                        assert!(
                            labels.contains(&expected),
                            "{game:?} missing common action {expected}"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn both_comparison_modals_fit_short_high_dpi_light_and_dark_windows_without_a_default_winner()
    -> anyhow::Result<()> {
        let snapshot = paired_snapshot()?;
        for game in [GameId::Dusklight, GameId::Totk] {
            let mut model = view(game, &snapshot);
            model.conflict = true;
            model.can_resolve = true;
            model.can_continue = true;
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
                                    screen_rect: Some(egui::Rect::from_min_size(
                                        egui::Pos2::ZERO,
                                        size,
                                    )),
                                    ..Default::default()
                                },
                                |context| {
                                    let (_, action) = prompt(context, &model, &mut choice);
                                    assert!(
                                        action.is_none(),
                                        "Rendering must not authorize replacement or play"
                                    );
                                },
                            );
                            for clipped in &output.shapes {
                                if let egui::Shape::Text(text) = &clipped.shape {
                                    let bounds = text.galley.rect.translate(text.pos.to_vec2());
                                    if clipped.clip_rect.intersects(bounds) {
                                        assert!(
                                            bounds.left() >= -1.0 && bounds.right() <= size.x + 1.0,
                                            "{game:?}: comparison overflow at {size:?}: {}",
                                            text.galley.job.text
                                        );
                                    }
                                    if size.y >= 400.0 && text.galley.job.text == "Not now" {
                                        assert!(
                                            bounds.top() >= 0.0 && bounds.bottom() <= size.y,
                                            "{game:?}: comparison actions left viewport"
                                        );
                                    }
                                }
                            }
                        }
                        assert_eq!(
                            choice, selected,
                            "Neither game may choose a branch without input"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn both_home_views_render_the_identical_action_and_section_contract() -> anyhow::Result<()> {
        let snapshot = paired_snapshot()?;
        for game in [GameId::Dusklight, GameId::Totk] {
            let model = view(game, &snapshot);
            for width in [360.0, 640.0, 960.0] {
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
                                egui::Pos2::ZERO,
                                Vec2::new(width, 3000.0),
                            )),
                            ..Default::default()
                        },
                        |context| {
                            egui::CentralPanel::default().show(context, |ui| {
                                home(ui, &model);
                            });
                        },
                    );
                    let mut labels = Vec::new();
                    for clipped in &output.shapes {
                        if let egui::Shape::Text(text) = &clipped.shape {
                            let bounds = text.galley.rect.translate(text.pos.to_vec2());
                            assert!(
                                bounds.left() >= -1.0 && bounds.right() <= width + 1.0,
                                "Shared home overflow for {game:?} at {width}: {}",
                                text.galley.job.text
                            );
                            labels.push(text.galley.job.text.as_str());
                        }
                    }
                    for label in [
                        "Sync now",
                        "Compare saves",
                        "History",
                        "Snapshot verification",
                        "Hearts",
                        "Rupees",
                        "Played",
                        "Save details",
                    ] {
                        assert!(
                            labels.contains(&label),
                            "Missing shared home control for {game:?}: {label}"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn both_games_use_the_same_responsive_save_card_contract() -> anyhow::Result<()> {
        let dusklight = save_data::inspect(&save_data::tests::populated(true)?)?;
        let totk = totk::Summary {
            files: 4,
            bytes: 2_945_388,
            album_files: 0,
            slots: vec![totk::SlotSummary {
                number: 0,
                files: 4,
                bytes: 2_945_388,
                hash: "a".repeat(64),
                details: Some(crate::totk_save::Details {
                    format: "1.4".into(),
                    max_hearts: Some(20.0),
                    rupees: Some(999_999),
                    play_seconds: Some(1_234_567),
                    autosave: Some(false),
                    saved_at: Some("2026-11-04 14:01".into()),
                    ..Default::default()
                }),
                details_error: None,
            }],
        };
        for cards in [dusklight_cards(&dusklight), totk_cards(&totk)] {
            assert!(!cards.is_empty());
            for width in [260.0, 360.0, 640.0, 960.0] {
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
                                egui::Pos2::ZERO,
                                Vec2::new(width, 2400.0),
                            )),
                            ..Default::default()
                        },
                        |context| {
                            egui::CentralPanel::default().show(context, |ui| progress(ui, &cards));
                        },
                    );
                    let mut labels = Vec::new();
                    for clipped in &output.shapes {
                        if let egui::Shape::Text(text) = &clipped.shape {
                            let bounds = text.galley.rect.translate(text.pos.to_vec2());
                            assert!(
                                bounds.left() >= -1.0 && bounds.right() <= width + 1.0,
                                "Save-card overflow at {width}: {}",
                                text.galley.job.text
                            );
                            labels.push(text.galley.job.text.as_str());
                        }
                    }
                    for label in ["Hearts", "Rupees", "Played", "Save details"] {
                        assert!(labels.contains(&label), "Missing common component {label}");
                    }
                }
            }
        }
        Ok(())
    }
}
