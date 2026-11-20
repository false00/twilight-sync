use crate::{files::ensure_safe_path, native, protocol::GameId, totk};
use anyhow::{Result, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub(crate) const ACCOUNT: u128 = 0x5a3c_9e1f_0b7d_2468_0000_0000_0000_0001;

pub fn default_install() -> Result<PathBuf> {
    crate::install::default_directory(GameId::Triforce)
}

pub fn ensure_stopped() -> Result<()> {
    native::Game::Triforce.ensure_stopped()
}

pub fn committed_save(user: &Path) -> Result<PathBuf> {
    ensure_safe_path(user)?;
    let transaction = user.join("save-tx").join(format!("{ACCOUNT:032x}"));
    ensure_safe_path(&transaction)?;
    if transaction.try_exists()? {
        for entry in fs::read_dir(transaction)? {
            let name = entry?.file_name();
            let name = name.to_string_lossy();
            ensure!(
                name != "old" && name != "new" && !name.starts_with("ready-"),
                "Triforce has an unfinished save commit; launch and close the game before syncing"
            );
        }
    }
    let root = user.join("save");
    ensure_safe_path(&root)?;
    Ok(root)
}

pub fn snapshot(user: &Path) -> Result<Option<totk::Snapshot>> {
    let root = committed_save(user)?;
    if !root.try_exists()? {
        return Ok(None);
    }
    ensure!(root.is_dir(), "Triforce committed save must be a directory");
    if !root.join(format!("{ACCOUNT:032x}")).try_exists()? {
        ensure!(
            fs::read_dir(&root)?.next().transpose()?.is_none(),
            "Triforce saves exist for an unsupported account; the manager will not guess which profile to replace"
        );
        return Ok(None);
    }
    totk::snapshot_account(&root, &ACCOUNT.to_le_bytes())
}

pub fn summary(bytes: &[u8]) -> Result<totk::Summary> {
    totk::summary_with_details(bytes, native::Game::Triforce)
}

pub fn apply(user: &Path, bytes: &[u8], expected: Option<&str>) -> Result<Option<PathBuf>> {
    totk::apply_native(user, bytes, expected, native::Game::Triforce)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use anyhow::Context;

    pub(crate) fn seed(user: &Path, marker: u8) -> Result<()> {
        let account = user.join("save").join(format!("{ACCOUNT:032x}"));
        let slot = account.join("slot_00");
        fs::create_dir_all(&slot)?;
        for (path, bytes) in [
            (account.join("account.sav"), vec![marker; 32]),
            (account.join("option.sav"), vec![marker; 64]),
            (slot.join("progress.sav"), vec![marker; 2048]),
            (slot.join("caption.sav"), vec![marker; 256]),
        ] {
            fs::write(path, bytes)?;
        }
        Ok(())
    }

    #[test]
    fn oot_projects_carry_progress_both_ways_and_retain_target_backups_and_settings() -> Result<()>
    {
        let temp = tempfile::tempdir()?;
        let triforce = temp.path().join("Triforce");
        let longshot = temp.path().join("Longshot");
        for (root, executable) in [(&triforce, "Triforce.exe"), (&longshot, "longshot.exe")] {
            fs::create_dir_all(root.join("bin"))?;
            fs::create_dir(root.join("romfs"))?;
            fs::write(root.join("image.bin"), b"synthetic game data")?;
            fs::write(
                root.join("bin").join(executable),
                b"fixture, never executed",
            )?;
        }
        seed(&triforce.join("user"), 7)?;
        seed(&longshot.join("user"), 2)?;
        let original_target =
            snapshot(&longshot.join("user"))?.context("Missing target fixture")?;
        fs::write(longshot.join("longshot_options.ini"), b"Longshot graphics")?;
        let transfers = temp.path().join("transfers");
        let mut channel = totk::Channel::for_game(
            transfers.clone(),
            Ok(triforce.clone()),
            native::Game::Triforce,
        );
        channel.refresh()?;
        channel.switch_oot_installation(&longshot)?;
        assert_eq!(
            channel.executable()?,
            longshot.join("bin").join("longshot.exe")
        );
        assert_eq!(
            snapshot(&longshot.join("user"))?.map(|save| save.hash),
            snapshot(&triforce.join("user"))?.map(|save| save.hash)
        );
        assert_eq!(
            fs::read(longshot.join("longshot_options.ini"))?,
            b"Longshot graphics"
        );
        let backup = fs::read_dir(longshot.join("user").join("save-sync-backups"))?
            .next()
            .context("Target save was not backed up")??
            .path();
        let backed_up = temp.path().join("backup-profile");
        fs::create_dir_all(&backed_up)?;
        fs::rename(&backup, backed_up.join("save"))?;
        assert_eq!(
            snapshot(&backed_up)?.map(|save| save.hash),
            Some(original_target.hash)
        );

        seed(&longshot.join("user"), 9)?;
        channel.switch_oot_installation(&triforce)?;
        assert_eq!(
            snapshot(&longshot.join("user"))?.map(|save| save.hash),
            snapshot(&triforce.join("user"))?.map(|save| save.hash)
        );
        let restarted = totk::Channel::for_game(transfers, Ok(longshot), native::Game::Triforce);
        assert_eq!(
            restarted.executable()?,
            triforce.join("bin").join("Triforce.exe")
        );
        Ok(())
    }

    #[test]
    fn oot_switches_protect_pending_sessions_and_prepare_uninstalled_projects() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let previous = temp.path().join("Triforce");
        let next = temp.path().join("Longshot");
        seed(&previous.join("user"), 7)?;
        let mut channel = totk::Channel::for_game(
            temp.path().join("transfers"),
            Ok(previous.clone()),
            native::Game::Triforce,
        );
        channel.status.remote_running = true;
        assert!(channel.switch_oot_installation(&next).is_err());
        assert!(!next.try_exists()?);
        channel.status.remote_running = false;
        channel.status.pending = true;
        assert!(channel.switch_oot_installation(&next).is_err());
        channel.status.pending = false;
        channel.switch_oot_installation(&next)?;
        assert!(!channel.status.installed);
        assert_eq!(
            snapshot(&previous.join("user"))?.map(|save| save.hash),
            snapshot(&next.join("user"))?.map(|save| save.hash)
        );

        let transaction = previous
            .join("user")
            .join("save-tx")
            .join(format!("{ACCOUNT:032x}"))
            .join("new");
        fs::create_dir_all(transaction)?;
        assert!(channel.switch_oot_installation(&previous).is_err());
        assert_eq!(channel.status.install_path, Some(next));
        Ok(())
    }

    #[test]
    fn full_committed_saves_seed_empty_pcs_and_preserve_unrelated_profiles_and_settings()
    -> Result<()> {
        for existing in [false, true] {
            let temp = tempfile::tempdir()?;
            let source = temp.path().join("source");
            let destination = temp.path().join("destination");
            seed(&source, 7)?;
            fs::create_dir_all(&destination)?;
            if existing {
                seed(&destination, 2)?;
                let inactive = destination.join("save").join("f".repeat(32));
                fs::create_dir_all(&inactive)?;
                fs::write(inactive.join("progress.sav"), b"inactive profile")?;
            }
            fs::write(destination.join("triforce_options.ini"), b"local graphics")?;
            let original = snapshot(&destination)?.map(|save| save.hash);
            let incoming = snapshot(&source)?.context("Missing synthetic source save")?;
            let backup = apply(&destination, &incoming.bytes, original.as_deref())?;
            assert_eq!(
                snapshot(&destination)?.map(|save| save.hash),
                Some(incoming.hash)
            );
            assert_eq!(
                fs::read(destination.join("triforce_options.ini"))?,
                b"local graphics"
            );
            assert!(!destination.join("account.uid").try_exists()?);
            assert!(
                destination
                    .join("save")
                    .join(format!("{ACCOUNT:032x}"))
                    .join("account.sav")
                    .is_file()
            );
            if existing {
                assert!(backup.is_some());
                assert_eq!(
                    fs::read(
                        destination
                            .join("save")
                            .join("f".repeat(32))
                            .join("progress.sav")
                    )?,
                    b"inactive profile"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn malformed_known_metadata_cannot_replace_committed_progress() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        seed(&source, 7)?;
        seed(&destination, 2)?;
        let before = snapshot(&destination)?.context("Missing destination fixture")?;
        let slot = source
            .join("save")
            .join(format!("{ACCOUNT:032x}"))
            .join("slot_00");
        let (mut progress, caption) = crate::triforce_save::tests::fixture(0);
        progress[0..4].fill(0);
        fs::write(slot.join("progress.sav"), progress)?;
        fs::write(slot.join("caption.sav"), caption)?;
        let incoming = snapshot(&source)?.context("Missing source fixture")?;
        assert!(summary(&incoming.bytes).is_err());
        assert!(apply(&destination, &incoming.bytes, Some(&before.hash)).is_err());
        assert_eq!(
            snapshot(&destination)?.map(|save| save.hash),
            Some(before.hash)
        );
        assert!(!destination.join("account.uid").try_exists()?);
        Ok(())
    }

    #[test]
    fn pending_transactions_and_unknown_active_profiles_fail_closed() -> Result<()> {
        let temp = tempfile::tempdir()?;
        seed(temp.path(), 1)?;
        let transaction = temp.path().join("save-tx").join(format!("{ACCOUNT:032x}"));
        fs::create_dir_all(transaction.join("work"))?;
        fs::write(
            transaction.join("work").join("progress.sav"),
            b"uncommitted",
        )?;
        let committed = snapshot(temp.path())?.context("Missing committed save")?;
        for name in ["new", "old", "ready-7"] {
            let pending = transaction.join(name);
            fs::create_dir(&pending)?;
            assert!(snapshot(temp.path()).is_err());
            fs::remove_dir(&pending)?;
        }
        assert_eq!(
            snapshot(temp.path())?.map(|save| save.hash),
            Some(committed.hash)
        );
        let unknown = temp
            .path()
            .join("unknown")
            .join("save")
            .join("e".repeat(32));
        fs::create_dir_all(&unknown)?;
        fs::write(unknown.join("account.sav"), b"unknown profile")?;
        assert!(snapshot(&temp.path().join("unknown")).is_err());
        Ok(())
    }
}
