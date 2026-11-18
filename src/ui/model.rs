use super::widgets::SaveCard;
use crate::{protocol::GameId, runtime::Snapshot, save_data, totk};
use anyhow::{Result, bail};

pub(super) const GAMES: [GameId; 3] = [GameId::Dusklight, GameId::Totk, GameId::Triforce];

pub(super) fn game(index: i32) -> Result<GameId> {
    match index {
        0 => Ok(GameId::Dusklight),
        1 => Ok(GameId::Totk),
        2 => Ok(GameId::Triforce),
        _ => bail!("Unknown game selection"),
    }
}

pub(super) fn index(game: GameId) -> i32 {
    match game {
        GameId::Dusklight => 0,
        GameId::Totk => 1,
        GameId::Triforce => 2,
    }
}

pub(super) fn title(game: GameId) -> &'static str {
    match game {
        GameId::Dusklight => "Twilight Princess",
        GameId::Totk => "Tears of the Kingdom",
        GameId::Triforce => "Ocarina of Time",
    }
}

pub(super) fn subtitle(game: GameId) -> &'static str {
    match game {
        GameId::Dusklight => "Dusklight · a twilight adventure",
        GameId::Totk => "TotK Recomp · a world beyond the clouds",
        GameId::Triforce => "Triforce · an adventure through time",
    }
}

pub(super) fn native(snapshot: &Snapshot, game: GameId) -> Option<&totk::Status> {
    match game {
        GameId::Dusklight => None,
        GameId::Totk => Some(&snapshot.totk),
        GameId::Triforce => Some(&snapshot.triforce),
    }
}

pub(super) fn local_hash(snapshot: &Snapshot, game: GameId) -> Option<&str> {
    native(snapshot, game).map_or(snapshot.local_hash.as_deref(), |state| {
        state.local_hash.as_deref()
    })
}

pub(super) fn installed(snapshot: &Snapshot, game: GameId) -> bool {
    native(snapshot, game).map_or(snapshot.settings.game_exe.is_some(), |state| {
        state.installed
    })
}

pub(super) fn management_available(snapshot: &Snapshot) -> bool {
    !snapshot.game_running
        && !snapshot.remote_game
        && snapshot.settings.play_lock.is_none()
        && !snapshot.install_busy
        && !snapshot.texture_busy
        && !snapshot.self_updates.preparing
}

pub(super) fn configuration_available(snapshot: &Snapshot) -> bool {
    !snapshot.install_busy
        && !snapshot.texture_busy
        && !snapshot.self_updates.preparing
        && snapshot.source.preferences_valid
}

pub(super) fn can_locate(snapshot: &Snapshot, game: GameId) -> bool {
    if let Some(state) = native(snapshot, game) {
        configuration_available(snapshot)
            && !state.running
            && !state.remote_running
            && !state.busy
            && !state.updating
            && !state.launch_pending
            && state.lock.is_none()
            && state.remote_lock.is_none()
    } else {
        management_available(snapshot)
            && !snapshot.pending_save
            && !snapshot.pending_iso
            && snapshot.transfer.is_empty()
    }
}

pub(super) fn can_install(snapshot: &Snapshot, game: GameId) -> bool {
    management_available(snapshot)
        && !snapshot.pending_save
        && !snapshot.pending_iso
        && !snapshot.pending_recovery
        && snapshot.transfer.is_empty()
        && match game {
            GameId::Dusklight => true,
            GameId::Triforce => false,
            GameId::Totk => {
                let state = &snapshot.totk;
                state.checked
                    && state.install_path.is_some()
                    && can_locate(snapshot, game)
                    && !state.pending
                    && !state.release_pending
                    && !state.recovery_pending
            }
        }
}

pub(super) fn app_update_blocker(snapshot: &Snapshot) -> Option<&'static str> {
    let native = [&snapshot.totk, &snapshot.triforce];
    if snapshot.game_running
        || snapshot.remote_game
        || native
            .iter()
            .any(|state| state.running || state.remote_running)
    {
        Some("Close running games on both PCs before updating Twilight Sync.")
    } else if snapshot.self_updates.checking
        || snapshot.self_updates.preparing
        || snapshot.install_busy
        || snapshot.texture_busy
    {
        Some("Finish the current check, installation or update first.")
    } else if snapshot.pending_save
        || snapshot.pending_iso
        || snapshot.pending_recovery
        || !snapshot.transfer.is_empty()
        || native
            .iter()
            .any(|state| state.busy || state.launch_pending || state.recovery_pending)
    {
        Some("Finish the pending transfer, launch or recovery first.")
    } else {
        None
    }
}

pub(super) fn source_blocker(snapshot: &Snapshot) -> Option<&'static str> {
    let native = [&snapshot.totk, &snapshot.triforce];
    if snapshot.game_running
        || snapshot.remote_game
        || native
            .iter()
            .any(|state| state.running || state.remote_running)
    {
        Some(
            "Close games on both PCs before building. Source review and tool preparation remain available.",
        )
    } else if snapshot.settings.play_lock.is_some() {
        Some(
            "A Dusklight play reservation is held. Open Twilight Princess on Play and recover the completed session first.",
        )
    } else if native.iter().any(|state| {
        state.lock.is_some()
            || state.remote_lock.is_some()
            || state.launch_pending
            || state.release_pending
    }) {
        Some(
            "A native-game play reservation is held. Recover the completed session on Play before changing its installation.",
        )
    } else if snapshot.pending_save
        || snapshot.pending_iso
        || snapshot.pending_recovery
        || !snapshot.transfer.is_empty()
        || native
            .iter()
            .any(|state| state.pending || state.busy || state.recovery_pending)
    {
        Some("Finish pending transfers, save choices and recovery before building.")
    } else if snapshot.install_busy || snapshot.texture_busy || snapshot.self_updates.preparing {
        Some("Wait for the current installation, tool setup or update.")
    } else {
        None
    }
}

fn native_session_blocker<'a>(snapshot: &Snapshot, state: &'a totk::Status) -> Option<&'a str> {
    if state.updating {
        Some("Game files are being installed or built. Play and save replacement are paused.")
    } else if let Some(error) = &state.verification_error {
        Some(error)
    } else if !state.checked {
        Some("Checking your installation and save…")
    } else if !state.installed {
        Some("Set up the game or locate an existing installation.")
    } else if state.running {
        Some("Playing on this PC. Save replacement is paused.")
    } else if state.remote_running {
        Some("Your other PC is playing. Save replacement is paused.")
    } else if state.launch_pending {
        Some("Waiting for your other PC to reserve play.")
    } else if state.release_pending && snapshot.ready {
        Some("Waiting for the completed play session to be acknowledged.")
    } else if state.pending {
        Some("An incoming save needs your Accept or Keep decision.")
    } else if state.busy {
        Some("A save transfer or comparison is in progress.")
    } else if state.recovery_pending {
        Some("A completed-session recovery needs your decision.")
    } else if state.lock.is_some() || state.remote_lock.is_some() {
        Some("Recover the completed play reservation before continuing.")
    } else if snapshot.self_updates.preparing {
        Some("Preparing a Twilight Sync update.")
    } else {
        None
    }
}

pub(super) fn play_blocker(snapshot: &Snapshot, game: GameId) -> Option<&str> {
    if let Some(state) = native(snapshot, game) {
        native_session_blocker(snapshot, state).or({
            if snapshot.ready
                && state.peer_checked
                && state.remote_installed
                && !state.remote_save_ready
            {
                Some("Your other PC's save is not verified yet.")
            } else if state.conflict {
                Some("Compare the two save branches or explicitly keep playing this PC's branch.")
            } else {
                None
            }
        })
    } else if !installed(snapshot, game) {
        Some("Set up Dusklight or locate an existing installation.")
    } else if snapshot.game_running {
        Some("Dusklight is running on this PC.")
    } else if snapshot.remote_game {
        Some("Your other PC is playing Dusklight.")
    } else if snapshot.settings.play_lock.is_some() {
        Some("Recover the completed Dusklight play reservation before continuing.")
    } else if snapshot.install_busy || snapshot.texture_busy || snapshot.self_updates.preparing {
        Some("Finish the current installation or update.")
    } else if !snapshot.save_ready
        || snapshot.save_profile.is_none()
        || snapshot.settings.save_file.is_none()
    {
        Some("The Dusklight save profile must be verified before protected play.")
    } else if snapshot.pending_save
        || snapshot.pending_iso
        || snapshot.pending_recovery
        || !snapshot.transfer.is_empty()
    {
        Some("Finish the pending transfer or save decision first.")
    } else if snapshot.conflict.is_some() {
        Some("Compare the two branches or choose Keep both, play here.")
    } else {
        None
    }
}

pub(super) fn sync_blocker(snapshot: &Snapshot, game: GameId) -> Option<&str> {
    if let Some(state) = native(snapshot, game) {
        native_session_blocker(snapshot, state).or_else(|| {
            if !snapshot.ready {
                Some("Offline. This PC's progress stays safely here.")
            } else if game == GameId::Triforce && !snapshot.triforce_supported {
                Some("Update both PCs for Triforce save sharing.")
            } else if !state.peer_checked || !state.remote_save_ready {
                Some("Waiting for your other PC's verified save state.")
            } else if !state.remote_installed {
                Some("Set up this game on your other PC too.")
            } else if state.local_hash.is_none() {
                Some("No committed save to send yet. You can receive one.")
            } else if state.local_hash == state.remote_hash {
                Some("Up to date. Both PCs have the same complete save.")
            } else {
                None
            }
        })
    } else if !snapshot.ready {
        Some("Offline. Play here and reconnect when you're ready.")
    } else if !snapshot.save_ready || snapshot.local_hash.is_none() {
        Some("No verified local save to send yet.")
    } else if snapshot.game_running || snapshot.remote_game || snapshot.settings.play_lock.is_some()
    {
        Some("Finish the play session before replacing saves.")
    } else if snapshot.pending_save
        || snapshot.pending_iso
        || snapshot.pending_recovery
        || !snapshot.transfer.is_empty()
        || snapshot.install_busy
        || snapshot.texture_busy
        || snapshot.self_updates.preparing
    {
        Some("Finish the current transfer, decision or installation.")
    } else if snapshot.local_hash == snapshot.remote_hash {
        Some("Up to date. Both PCs have the same complete save.")
    } else {
        None
    }
}

pub(super) fn can_restore(snapshot: &Snapshot, game: GameId) -> bool {
    if let Some(state) = native(snapshot, game) {
        state.installed
            && !state.updating
            && !state.running
            && !state.remote_running
            && !state.busy
            && !state.launch_pending
            && state.lock.is_none()
            && state.remote_lock.is_none()
            && state.verification_error.is_none()
            && !snapshot.self_updates.preparing
    } else {
        management_available(snapshot)
            && snapshot.save_ready
            && snapshot.save_profile.is_some()
            && !snapshot.pending_save
            && !snapshot.pending_iso
            && !snapshot.pending_recovery
            && snapshot.transfer.is_empty()
    }
}

pub(super) fn can_continue(snapshot: &Snapshot, game: GameId) -> bool {
    if let Some(state) = native(snapshot, game) {
        state.installed
            && !state.updating
            && !state.running
            && !state.remote_running
            && (!state.busy || state.pending)
            && !state.launch_pending
            && state.lock.is_none()
            && state.remote_lock.is_none()
            && state.verification_error.is_none()
            && !snapshot.self_updates.preparing
    } else {
        management_available(snapshot)
            && snapshot.save_ready
            && snapshot.save_profile.is_some()
            && snapshot.settings.save_file.is_some()
            && !snapshot.pending_iso
            && !snapshot.pending_recovery
            && snapshot.transfer.is_empty()
    }
}

pub(super) fn can_resolve(snapshot: &Snapshot, game: GameId) -> bool {
    if let Some(state) = native(snapshot, game) {
        snapshot.ready && state.comparison_verified && can_restore(snapshot, game)
    } else {
        snapshot.ready
            && can_restore(snapshot, game)
            && snapshot
                .conflict
                .as_ref()
                .and_then(|conflict| conflict.remote.as_ref())
                .zip(snapshot.local_save.as_ref())
                .is_some_and(|(remote, local)| {
                    remote.game == local.game && remote.file_name == local.file_name
                })
    }
}

pub(super) fn dusklight_cards(summary: &save_data::Summary) -> Vec<SaveCard> {
    summary
        .slots
        .iter()
        .map(|slot| {
            let mut inventory = slot.equipment.clone();
            inventory.extend(slot.items.iter().map(|item| item.name.clone()));
            SaveCard {
                title: format!("Slot {} · {}", slot.number, slot.player).into(),
                kind: if slot.wolf { "Wolf form" } else { "Human form" }.into(),
                date: slot
                    .saved_at
                    .as_deref()
                    .map(|date| format!("{date} UTC"))
                    .unwrap_or_else(|| "Save date unavailable".into())
                    .into(),
                hearts: slot.hearts.to_string().into(),
                rupees: slot.rupees.to_string().into(),
                played: slot
                    .play_seconds
                    .map(save_data::duration_label)
                    .unwrap_or_else(|| "Unavailable".into())
                    .into(),
                extra: format!(
                    "{}\n{} · Room {} · {} heart pieces · {} Poes · {} insects\n{}",
                    slot.story(),
                    slot.stage,
                    slot.room,
                    slot.heart_pieces,
                    slot.poes,
                    slot.insects,
                    inventory.join(", ")
                )
                .into(),
                warning: "".into(),
            }
        })
        .collect()
}

pub(super) fn native_cards(summary: &totk::Summary) -> Vec<SaveCard> {
    let mut slots: Vec<_> = summary.slots.iter().collect();
    slots.sort_by(|left, right| {
        right
            .details
            .as_ref()
            .and_then(|d| d.saved_at.as_ref())
            .cmp(&left.details.as_ref().and_then(|d| d.saved_at.as_ref()))
            .then(left.number.cmp(&right.number))
    });
    slots
        .into_iter()
        .map(|slot| {
            let details = slot.details.as_ref();
            let unavailable = || "Unavailable".to_string();
            let mut extra = vec![format!(
                "{} files · {:.1} MiB",
                slot.files,
                slot.bytes as f64 / 1_048_576.0
            )];
            if let Some(details) = details {
                if let Some(stamina) = details.stamina_wheels {
                    extra.push(format!("Stamina {stamina:.1} wheels"));
                }
                if let Some(map) = &details.map {
                    extra.push(format!("Location {map}"));
                }
                if let Some([x, y, z]) = details.position {
                    extra.push(format!("Position {x:.0}, {y:.0}, {z:.0}"));
                }
                extra.push(format!("Save format {}", details.format));
            }
            SaveCard {
                title: format!("Slot {}", slot.number + 1).into(),
                kind: match details.and_then(|d| d.autosave) {
                    Some(true) => "Autosave",
                    Some(false) => "Manual save",
                    None => "Save slot",
                }
                .into(),
                date: details
                    .and_then(|d| d.saved_at.as_ref())
                    .map(|date| format!("{date} · game clock"))
                    .unwrap_or_else(|| "Save date unavailable".into())
                    .into(),
                hearts: details
                    .and_then(|d| d.max_hearts)
                    .map(|value| format!("{value}"))
                    .unwrap_or_else(unavailable)
                    .into(),
                rupees: details
                    .and_then(|d| d.rupees)
                    .map(|value| value.to_string())
                    .unwrap_or_else(unavailable)
                    .into(),
                played: details
                    .and_then(|d| d.play_seconds)
                    .map(|value| save_data::duration_label(u64::from(value)))
                    .unwrap_or_else(unavailable)
                    .into(),
                extra: extra.join(" · ").into(),
                warning: slot.details_error.clone().unwrap_or_default().into(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot() -> Result<Snapshot> {
        let backend = crate::runtime::ui_smoke_backend("ready")?;
        let snapshot = backend.snapshot()?;
        backend.command(crate::runtime::Command::Shutdown)?;
        Ok(snapshot)
    }

    #[test]
    fn all_game_safety_guards_preserve_reservations_and_active_work() -> Result<()> {
        let mut snapshot = snapshot()?;
        snapshot.totk = totk::Status {
            checked: true,
            installed: true,
            ..Default::default()
        };
        snapshot.triforce = snapshot.totk.clone();
        assert!(play_blocker(&snapshot, GameId::Totk).is_none());
        snapshot.triforce.updating = true;
        assert!(play_blocker(&snapshot, GameId::Triforce).is_some());
        assert!(can_continue(&snapshot, GameId::Totk));
        snapshot.triforce.updating = false;
        snapshot.triforce.running = true;
        assert!(app_update_blocker(&snapshot).is_some());
        assert!(source_blocker(&snapshot).is_some());
        snapshot.triforce.running = false;
        snapshot.settings.play_lock = Some(crate::config::PlayLock {
            owner: snapshot.public_key.clone(),
            token: "a".repeat(64),
        });
        assert!(app_update_blocker(&snapshot).is_none());
        assert!(source_blocker(&snapshot).is_some());
        assert!(configuration_available(&snapshot));
        assert!(!can_install(&snapshot, GameId::Dusklight));
        Ok(())
    }

    #[test]
    fn received_metadata_does_not_enable_a_branch_choice_without_verification() -> Result<()> {
        let mut snapshot = snapshot()?;
        snapshot.triforce = totk::Status {
            checked: true,
            installed: true,
            conflict: true,
            ..Default::default()
        };
        assert!(!can_resolve(&snapshot, GameId::Triforce));
        snapshot.triforce.comparison_verified = true;
        assert!(can_resolve(&snapshot, GameId::Triforce));
        snapshot.triforce.running = true;
        assert!(!can_resolve(&snapshot, GameId::Triforce));
        Ok(())
    }
}
