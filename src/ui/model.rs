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

pub(super) fn saves_match(snapshot: &Snapshot, game: GameId) -> bool {
    snapshot.ready
        && if let Some(state) = native(snapshot, game) {
            !state.updating
                && state.remote_save_ready
                && state.verification_error.is_none()
                && !state.running
                && !state.remote_running
                && state.local_hash.is_some()
                && state.local_hash == state.remote_hash
        } else {
            snapshot.save_ready
                && !snapshot.game_running
                && !snapshot.remote_game
                && snapshot.local_hash.is_some()
                && snapshot.local_hash == snapshot.remote_hash
        }
}

pub(super) fn game_status(snapshot: &Snapshot, game: GameId) -> &'static str {
    let (running, remote_running, pending, recovery, reserved, conflict) =
        if let Some(state) = native(snapshot, game) {
            if state.updating {
                return "Installation in progress";
            }
            if state.verification_error.is_some() {
                return "Verification needs attention";
            }
            (
                state.running,
                state.remote_running,
                state.pending,
                state.recovery_pending,
                state.lock.is_some() || state.remote_lock.is_some() || state.release_pending,
                state.conflict,
            )
        } else {
            (
                snapshot.game_running,
                snapshot.remote_game,
                snapshot.pending_save,
                snapshot.pending_recovery,
                snapshot.settings.play_lock.is_some(),
                snapshot.conflict.is_some(),
            )
        };
    if running {
        "Playing on this PC"
    } else if remote_running {
        "Playing on your other PC"
    } else if recovery {
        "Recovery needs approval"
    } else if pending {
        "Incoming save needs approval"
    } else if reserved {
        "Previous session needs attention"
    } else if conflict {
        "Two save branches to compare"
    } else if saves_match(snapshot, game) {
        "Saves synchronized"
    } else if !installed(snapshot, game) {
        "Ready to set up"
    } else if !snapshot.ready {
        "Offline play"
    } else if local_hash(snapshot, game).is_none() {
        "No committed save yet"
    } else {
        "Ready for your next chapter"
    }
}

pub(super) fn network_trusted(snapshot: &Snapshot) -> bool {
    snapshot.network.as_ref().is_some_and(|network| {
        snapshot.settings.network.as_deref() == Some(network.fingerprint.as_str())
    })
}

pub(super) fn can_pair(snapshot: &Snapshot) -> bool {
    network_trusted(snapshot)
        && snapshot.settings.peer.is_none()
        && !snapshot.connecting
        && !snapshot.ready
        && management_available(snapshot)
}

pub(super) fn can_reconnect(snapshot: &Snapshot) -> bool {
    network_trusted(snapshot)
        && !snapshot.ready
        && !snapshot.connecting
        && snapshot.settings.peer.as_ref().is_some_and(|peer| {
            snapshot
                .devices
                .iter()
                .any(|device| device.key == peer.public_key)
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
    if snapshot.game_running || native.iter().any(|state| state.running) {
        Some("Close running games on this PC before updating Twilight Sync.")
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
    } else if snapshot.textures.recovery_needed {
        Some("Recover the interrupted texture change in Textures before playing.")
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
            && snapshot.settings.game_exe.is_some()
            && !snapshot.textures.recovery_needed
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
            && management_available(snapshot)
            && snapshot.save_ready
            && snapshot.save_profile.is_some()
            && snapshot.settings.save_file.is_some()
            && !snapshot.pending_iso
            && !snapshot.pending_recovery
            && snapshot.transfer.is_empty()
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

pub(super) fn comparison_blocker(snapshot: &Snapshot, game: GameId) -> Option<&str> {
    if can_resolve(snapshot, game) {
        None
    } else if !snapshot.ready {
        Some("Reconnect both PCs before choosing a branch. Your local progress is retained.")
    } else if let Some(state) = native(snapshot, game) {
        native_session_blocker(snapshot, state).or(Some(
            "Waiting for both complete, verified, compatible save snapshots.",
        ))
    } else if snapshot.game_running || snapshot.remote_game {
        Some("Close Dusklight on both PCs before choosing a save branch.")
    } else if snapshot.settings.play_lock.is_some() {
        Some("Recover the completed play reservation before choosing a branch.")
    } else if snapshot.install_busy || snapshot.texture_busy || snapshot.self_updates.preparing {
        Some("Finish the current installation or update before choosing a branch.")
    } else if snapshot.pending_iso || snapshot.pending_recovery || !snapshot.transfer.is_empty() {
        Some("Finish the pending transfer or recovery before choosing a branch.")
    } else {
        Some("Waiting for both complete, verified snapshots with compatible save identities.")
    }
}

pub(super) fn can_accept(snapshot: &Snapshot, game: GameId) -> bool {
    if let Some(state) = native(snapshot, game) {
        state.pending
            && !state.running
            && !state.remote_running
            && !state.updating
            && !state.launch_pending
            && state.lock.is_none()
            && state.remote_lock.is_none()
            && state.verification_error.is_none()
            && !snapshot.self_updates.preparing
    } else {
        snapshot.pending_save
            && management_available(snapshot)
            && snapshot.save_ready
            && snapshot.save_profile.is_some()
            && !snapshot.pending_iso
            && snapshot.transfer.is_empty()
    }
}

pub(super) fn can_recover(snapshot: &Snapshot, game: GameId) -> bool {
    if let Some(state) = native(snapshot, game) {
        !state.running
            && !state.remote_running
            && !state.updating
            && !state.launch_pending
            && !state.pending
            && !snapshot.self_updates.preparing
    } else {
        !snapshot.game_running
            && !snapshot.remote_game
            && !snapshot.install_busy
            && !snapshot.texture_busy
            && !snapshot.self_updates.preparing
            && !snapshot.pending_save
            && !snapshot.pending_iso
            && snapshot.transfer.is_empty()
    }
}

pub(super) fn can_change_textures(snapshot: &Snapshot) -> bool {
    management_available(snapshot)
        && snapshot.settings.texture_profile.is_some()
        && !snapshot.textures.recovery_needed
}

pub(super) fn texture_blocker(snapshot: &Snapshot) -> Option<&'static str> {
    if snapshot.texture_busy {
        Some("Wait for the current texture installation or switch to finish.")
    } else if snapshot.textures.recovery_needed {
        Some("Recover the interrupted texture switch before installing or activating another pack.")
    } else if snapshot.settings.game_exe.is_none() {
        Some("Locate or install Dusklight in Game setup before installing textures.")
    } else if snapshot.game_running || snapshot.remote_game {
        Some("Close Dusklight on both PCs before changing its textures.")
    } else if snapshot.settings.play_lock.is_some() {
        Some("Finish or recover the Dusklight play reservation before changing textures.")
    } else if snapshot.install_busy || snapshot.self_updates.preparing {
        Some("Finish the current installation or app update before changing textures.")
    } else if snapshot.settings.texture_profile.is_none() {
        Some("Refresh the library to detect Dusklight's profile before activating a pack.")
    } else {
        None
    }
}

pub(super) fn can_import_textures(snapshot: &Snapshot) -> bool {
    management_available(snapshot)
        && snapshot.settings.game_exe.is_some()
        && !snapshot.textures.recovery_needed
}

pub(super) fn can_send_disc(snapshot: &Snapshot) -> bool {
    snapshot.ready
        && snapshot.settings.iso_enabled
        && snapshot.settings.iso_file.is_some()
        && management_available(snapshot)
        && !snapshot.pending_save
        && !snapshot.pending_iso
        && !snapshot.pending_recovery
        && snapshot.transfer.is_empty()
}

fn game_file_idle(snapshot: &Snapshot, kind: crate::game_file::Kind) -> bool {
    management_available(snapshot)
        && snapshot.source.preferences_valid
        && native(snapshot, kind.game()).is_some_and(|state| {
            !state.running
                && !state.remote_running
                && !state.busy
                && !state.updating
                && !state.launch_pending
                && !state.recovery_pending
        })
}

pub(super) fn can_send_game_file(snapshot: &Snapshot, kind: crate::game_file::Kind) -> bool {
    let preference = snapshot.source.preferences.get(kind.game());
    let path = match kind {
        crate::game_file::Kind::OotNup | crate::game_file::Kind::TotkBase => &preference.game_input,
        crate::game_file::Kind::TotkUpdate => &preference.update,
        crate::game_file::Kind::TotkKeys => &preference.keys,
    };
    snapshot.ready
        && snapshot.game_files_supported
        && snapshot.game_file_permissions.allows(kind)
        && game_file_idle(snapshot, kind)
        && path.as_deref().is_some_and(|path| kind.matches_path(path))
        && !snapshot.pending_save
        && !snapshot.pending_iso
        && !snapshot.pending_recovery
        && snapshot.transfer.is_empty()
}

pub(super) fn can_accept_disc(snapshot: &Snapshot) -> bool {
    snapshot.ready
        && snapshot.pending_iso
        && snapshot
            .pending_disc_format
            .is_some_and(|format| match format {
                crate::game_file::Format::Game(kind) => {
                    snapshot.game_files_supported
                        && snapshot.game_file_permissions.allows(kind)
                        && game_file_idle(snapshot, kind)
                }
                _ => snapshot.settings.iso_enabled,
            })
        && management_available(snapshot)
        && !snapshot.pending_save
        && !snapshot.pending_recovery
        && snapshot.transfer.is_empty()
}

pub(super) fn dusklight_cards(summary: &save_data::Summary) -> Vec<SaveCard> {
    let mut slots: Vec<_> = summary.slots.iter().collect();
    slots.sort_by(|left, right| {
        right
            .saved_at
            .cmp(&left.saved_at)
            .then(left.number.cmp(&right.number))
    });
    slots
        .into_iter()
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
                    "{}\nPlayer: {} · Horse: {}\nLocation: {} / room {}\nHeart pieces: {} / 5 · Poe souls: {} / 60 · Golden insects: {} / 24\nFused Shadows: {} / 4 · Mirror shards: {} / 4\nDeaths: {} · Clears: {}\nAmmunition: {} arrows / bombs {}, {}, {}\nEquipment & inventory: {}",
                    slot.story(),
                    slot.player,
                    slot.horse,
                    slot.stage,
                    slot.room,
                    slot.heart_pieces,
                    slot.poes,
                    slot.insects,
                    slot.fused_shadows,
                    slot.mirror_shards,
                    slot.deaths,
                    slot.clears,
                    slot.arrows,
                    slot.bombs[0],
                    slot.bombs[1],
                    slot.bombs[2],
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
    use anyhow::Context;
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

    #[test]
    fn reconnect_requires_the_pinned_partner_on_the_approved_network() -> Result<()> {
        let mut snapshot = snapshot()?;
        snapshot.ready = false;
        snapshot.devices.push(crate::lan::Discovered {
            key: "a".repeat(64),
            name: "Unverified announcement".into(),
            address: "192.168.10.21:42817".parse()?,
            pairing: true,
            seen: std::time::Instant::now(),
            game_channels_seen: None,
            triforce_seen: None,
            game_files_seen: None,
        });
        assert!(!can_reconnect(&snapshot));
        snapshot.devices[0].key = snapshot
            .settings
            .peer
            .as_ref()
            .context("Missing preview peer")?
            .public_key
            .clone();
        assert!(can_reconnect(&snapshot));
        snapshot.settings.network = None;
        assert!(!can_reconnect(&snapshot));
        assert!(!can_pair(&snapshot));
        Ok(())
    }

    #[test]
    fn game_file_receiving_uses_its_own_permission_without_breaking_disc_offers() -> Result<()> {
        use crate::game_file::{Format, Kind};
        let mut snapshot = snapshot()?;
        snapshot.settings.iso_enabled = true;
        snapshot.settings.iso_file = Some("Twilight Princess.iso".into());
        assert!(can_send_disc(&snapshot));
        snapshot.settings.iso_enabled = false;
        snapshot.pending_iso = true;
        snapshot.pending_disc_format = Some(Format::Game(Kind::OotNup));
        snapshot.game_files_supported = true;
        assert!(!can_accept_disc(&snapshot));
        snapshot.game_file_permissions.oot = true;
        assert!(can_accept_disc(&snapshot));
        snapshot.pending_disc_format = Some(Format::Game(Kind::TotkKeys));
        snapshot.game_file_permissions.totk = true;
        assert!(!can_accept_disc(&snapshot));
        snapshot.game_file_permissions.keys = true;
        assert!(can_accept_disc(&snapshot));
        snapshot.totk.running = true;
        assert!(!can_accept_disc(&snapshot));
        Ok(())
    }

    #[test]
    fn incoming_save_and_texture_controls_respect_active_sessions_and_recovery() -> Result<()> {
        let mut snapshot = snapshot()?;
        snapshot.pending_save = true;
        assert!(can_accept(&snapshot, GameId::Dusklight));
        snapshot.game_running = true;
        assert!(!can_accept(&snapshot, GameId::Dusklight));
        assert!(!can_recover(&snapshot, GameId::Dusklight));
        assert!(!can_import_textures(&snapshot));
        snapshot.game_running = false;
        snapshot.pending_save = false;
        assert!(can_change_textures(&snapshot));
        snapshot.textures.recovery_needed = true;
        assert!(!can_change_textures(&snapshot));
        assert!(!can_import_textures(&snapshot));
        assert!(play_blocker(&snapshot, GameId::Dusklight).is_some());
        assert!(!can_continue(&snapshot, GameId::Dusklight));
        snapshot.triforce.pending = true;
        assert!(can_accept(&snapshot, GameId::Triforce));
        snapshot.triforce.remote_running = true;
        assert!(!can_accept(&snapshot, GameId::Triforce));
        assert!(!can_recover(&snapshot, GameId::Triforce));
        Ok(())
    }

    #[test]
    fn pending_dusklight_saves_can_be_compared_without_enabling_history_restore() -> Result<()> {
        let backend = crate::runtime::ui_smoke_backend("attention")?;
        let mut snapshot = backend.snapshot()?;
        backend.command(crate::runtime::Command::Shutdown)?;
        assert!(snapshot.pending_save);
        assert!(can_resolve(&snapshot, GameId::Dusklight));
        assert!(!can_restore(&snapshot, GameId::Dusklight));
        assert!(comparison_blocker(&snapshot, GameId::Dusklight).is_none());
        snapshot.settings.game_exe = None;
        assert!(can_resolve(&snapshot, GameId::Dusklight));
        assert!(!can_continue(&snapshot, GameId::Dusklight));
        snapshot.pending_iso = true;
        assert!(!can_resolve(&snapshot, GameId::Dusklight));
        snapshot.pending_iso = false;
        snapshot
            .conflict
            .as_mut()
            .context("Missing conflict")?
            .remote
            .as_mut()
            .context("Missing remote summary")?
            .file_name = "incompatible-save".into();
        assert!(!can_resolve(&snapshot, GameId::Dusklight));
        Ok(())
    }

    #[test]
    fn dusklight_cards_retain_the_original_metadata_and_latest_save_order() -> Result<()> {
        let snapshot = snapshot()?;
        let mut summary = snapshot.local_save.context("Missing synthetic save")?;
        let mut newer = summary.slots[0].clone();
        newer.number = 2;
        newer.saved_at = Some("2026-10-26T14:15:00Z".into());
        newer.clears = 3;
        summary.slots.push(newer);
        let cards = dusklight_cards(&summary);
        assert!(cards[0].title.starts_with("Slot 2"));
        for detail in [
            "Horse: Epona",
            "Fused Shadows: 3 / 4",
            "Mirror shards: 1 / 4",
            "Deaths: 7",
            "Clears: 3",
            "30 arrows / bombs 24, 10, 0",
            "Hylian Shield",
        ] {
            assert!(cards[0].extra.contains(detail), "Missing {detail}");
        }
        Ok(())
    }
}
